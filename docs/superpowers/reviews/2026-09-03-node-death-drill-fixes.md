# Node-death drill: the four findings, fixed

Branch `drill-fixes`, four commits off `1da91d27`. Gates below are real, unpiped exit codes.

## F5 — `NodeDead` was never cleared (`47f82a56`)

`sweep_volumes` → `mark_parent` writes `Degraded=True/NodeDead` on every parent of a dead owner's
volume, and nothing ever removed it. The stopped arms rebuild conditions with `replaced()`, which
only rewrites its own type, so the mark outlived the node's recovery; `/v1`'s `interrupted()` keys
on exactly that condition, hence a permanent 409 on `start`.

`cleared_node_dead` (`bins/agent/src/controller/workspace.rs`) filters it out at both stopped-arm
condition builds in `stop_workspace` and at `stop_environment`'s already-stopped recompute
(`bins/agent/src/controller/environment.rs`). Safe because both parent controllers watch with
`fields("status.nodeName={me}")` — the owner reconciling its own object IS the proof its node is
alive, and no other node reaches these arms.

The volume half needed no change: `apply_volume`'s `returned` escape re-runs the pass on
`phase == Unavailable`, and the terminal write replaces `st.conditions` wholesale.

Test: `a_stopped_parent_reconciled_by_its_owner_drops_node_dead` (`bins/agent/tests/reconcile.rs`).

## F3 — a dead node kept sweeping, and its status write lost races (`0f2f9c3c`)

**(a)** `sweep_volumes`'s Volume status write is a PUT carrying `resourceVersion` and races the
owner's own controller; a 409 only warned, leaving a dead owner's volume `Available=True` until
something else happened to touch it. Now the same re-read-and-retry loop (bound 3) that
`mark_parent_of` and `write_replica_status` already use.

**(b)** `pull_beat_with` now returns early when `node_is_dead` says the cluster reads THIS node as
dead — the agent kept reaping, unclaiming and retiring right through a kubelet outage, on a view no
other node shared, while every live node was already doing that work correctly. `node_is_dead`,
never `unplaceable`: a decommissioning node is alive and must keep sweeping or its drain stalls
forever. The 180 s floor is the only guard, and the comment says so: a node wrongly NotReady past
it stops reconciling until its Node object recovers. That is the deliberate trade — a wrong sweep
deletes data, a paused one only waits.

Tests: `the_sweep_retries_a_conflicted_volume_status_write`,
`a_node_the_cluster_reads_as_dead_sweeps_nothing`.

## F4 — no source, and a full tick before retrying (`5006bd5c`)

`pull_volume`'s sources came only from `beat.replicas`, so a standby with no row for the owner (a
fresh volume, or a row the reaper took) had an empty source list and could never fetch the commit
it was placed for. The owner is appended LAST — a Synced peer still wins — and only when it is in
`live`, so a genuinely dead owner costs no failed dial per commit per pass.

`pull_volume` now returns its already-computed `any_pull_failed`, OR'd up through the volume loop
and out of `pull_beat`. `after_pass(&wake, missed)` gained `Next::RetrySoon`, and `spawn_pull`
selects on a 30 s sleep for it — a pending wake still wins, so a stop's flush gate is never delayed
by a retry.

Tests: `the_owner_is_a_last_resort_source_only_while_it_is_live`,
`a_pass_that_missed_a_commit_retries_soon_unless_a_wake_is_pending`.

## F2 — orphaned `VolumeReplica` rows (`54df34c9`)

Every other arm of `retire_pass` walks LISTED volumes, so a row whose Volume CR is gone was never
revisited: it outlived the workspace, and its stale `Synced` still satisfied a stop's flush gate
and won claims. Three lines beside the `orphan_voldirs` sweep already running there, this node's
own rows only — another node's rows are its own business and it runs the same sweep.

No ownerReference, marked `// ponytail:` in place: the sweep has to exist anyway for the rows
already out there, `write_replica_status` has no Volume UID to hand without a GET per row it
creates, and a second delete path with different timing during a takeover is worse than one sweep.

Test: `retire_pass_drops_my_replica_row_whose_volume_cr_is_gone`.

## RBAC

**None of the four change RBAC.** Every verb is already in `deploy/k3s/agent-rbac.yaml`:
`volumereplicas` delete (the reaper and `retire_pass` already do it), `volumes/status` and the
parents' `/status` update (the sweep already does it), `nodes` get/list (F3(b) reads the list the
pass already makes). `agent-admission.yaml` is untouched — nothing here writes a parent or volume
**spec**.

## Gates

```
cargo clippy --workspace -- -D warnings   → exit 0
cargo test                                → exit 0 (71 result lines, all ok, 0 failed)
```

`tests/ws_e2e.sh` was NOT run: it needs a Linux VM with btrfs and a k3s cluster, not this Mac.

## What the drill script should do differently

1. Assert the condition is **cleared**, not just that it appears — poll for `Degraded/NodeDead` to
   vanish and for `/v1 start` to stop 409ing once the node is Ready again, with a bound. Today it
   only asserts the marking, which is why F5 survived a passing drill.
2. **Kill the kubelet, not the node or the agent**, for at least one pass. F3 only reproduces when
   the node reads NotReady while its agent keeps writing status.
3. **Time stop → `Replicated=True` against a ceiling** (~30 s). 282 s reads as "slow but working"
   unless something actually fails on it.
4. **Diff `volumereplicas` against `volumes` at teardown**, after the workspaces are deleted. That
   turns F2 from an eyeball finding into an assertion.
5. **Dump the objects on failure** instead of tidying up: three of the four were diagnosed from
   conditions and log lines.
6. **Check host disk before a run.** The first attempt at this work died of ENOSPC.

## Residual concerns

- F3(b): a node wrongly NotReady past the floor stops reconciling until its Node object recovers.
  Intended, and the floor is the only thing bounding it.
- F4: the owner is skipped when it is not in `live`, so the dead-owner dial cost is gone — but a
  live owner that is merely unreachable still costs one failed dial per commit per pass.

---

# Round 2

Review verdict on the above: F2 and F4 stand; three fixes. All three below, one commit each.

## The self-dead guard was only half applied (`f8fa85a2`)

`pull_beat_with` bailing was not enough. The parent and volume reconcilers still ran every 15 s on
a partitioned agent — kubelet down, API server still reachable — and every pass wrote status: the
stopped arms cleared the sweep's `Degraded=True/NodeDead` (the fix from F5, working exactly as
written, on a node that had no business writing at all) and `apply_volume`'s `Unavailable` re-run
rewrote the volume's conditions. The mark another node had just written was therefore absent ~95%
of the time, and `/v1` accepted `start` on a node the cluster reads as dead.

`controller::i_am_dead` (`bins/agent/src/controller/mod.rs`) — one `get_opt` on our own Node per
reconcile, `node_is_dead` against the same `WS_NODE_DEAD_SECS` floor — is now the first thing
`apply_workspace`, `apply_environment` and `apply_volume` do, above every write and above
`cleared_node_dead`, returning `requeue(TICK)`. Keep-biased on a failed read: a Node we cannot
fetch is not evidence of death, and pausing on a hiccup would stall every workspace on the node.
`node_is_dead`, never `unplaceable` — a decommissioning node is alive and must keep converging.

Test: `a_parent_whose_own_node_is_dead_is_not_reconciled_at_all` — the recorder shows the single
Node read and nothing else. Two existing tests asserted exact call sequences and now account for
that read (`a_wrong_owner_label_is_re_stamped_from_spec`,
`a_second_reconcile_of_a_settled_workspace_writes_nothing`).

No RBAC change: `nodes` already carries `get,list,patch` in `deploy/k3s/agent-rbac.yaml`.

## `RetrySoon` was unbounded and node-wide (`6f1e1b21`)

The missed flag is per-PASS, so one permanently unfetchable Snapshot — the only source gone for
good — kept every node placed on that volume beating every 30 s forever, paying the whole listing
cost each time. `Next::RetrySoon` now carries its delay: `RETRY_SOON` doubled per CONSECUTIVE
missed pass, capped at `replica_interval`, so the worst case is exactly today's steady state. Any
clean pass resets the streak to zero, and the pending wake still wins.

Test: `consecutive_misses_back_off_to_the_ordinary_tick_and_one_clean_pass_resets` asserts the
30/60/120/240/300/300 sequence and the reset.

## The sweep's retry bound, explained (`1e4beb23`)

Comment only. Three attempts is enough only because the reconcilers no longer write back: against
an owner rewriting the same status every 15 s no bound would have been enough; against one-shot
writers, three is plenty. Said so where the loop is.

## Gates (round 2)

```
cargo clippy --workspace -- -D warnings   → exit 0
cargo test                                → exit 0 (71 result lines, all ok, 0 failed)
```

# Round 3

Two findings off a live drill of the stop/interrupt/decommission work.

## F6 — a clone of an INTERRUPTED workspace could never start (`035bd9e5`)

`/v1` wrote the clone as `VolumeSource::CloneOf { volume: <source>, commit: Some(<held cut>) }`,
which is the SHARED-WORKTREE path: `resolve_volume` makes the clone a second worktree of the
source's own `Volume`. That Volume is pinned to the node that died, so the peer that claims the
clone — the one node holding the cut — settles `Error / Degraded=NodeMismatch`. Observed twice on
the cluster. The spec's promise ("clone from the last synced point") needs the clone to have a
volume of its OWN.

`VolumeSource::SeededFrom { volume, snapshot }` is that volume: created by `ensure_child_volume`
like any fresh workspace, pinned to the claiming node, and materialized by
`Engine::seed_from_snapshot` — one `btrfs subvolume snapshot` of `pool.snap(volume, snapshot)` into
`pool.live(new_id)`, erroring `NO_SUCH_RECORD` (permanent, via `permanent_reason`) when the cut is
not held here. From there it is byte-for-byte the shipped `CloneOf{commit: None}` fresh-child path:
`migrate_volume` renames `live` into `live/{id}` and mints the baseline `Snapshot`.

`claim::decide` admits a node for such a parent only when its OWN `VolumeReplica` row for `volume`
names that exact cut in `status.branches` — `clone_source` now returns the pinned cut and
`placement` uses it as the bar instead of listing the newest transient cluster-wide. That matters:
on a dead owner the newest cut cluster-wide may be one the dead node made and nobody holds, which
is the same stranding in a different costume. The owner arm of `may_claim` is untouched and is
asserted, but it is not the case that fires here — the source volume's owner is the DEAD node, and
`decide` only gets past its owner guard because that owner is `unplaceable`.

A seeded clone carries BYTES only, no push history from the source: it owns a fresh `Volume` whose
chain begins at the migration baseline `migrate_volume` mints, so `history`/`refs` on it start
there rather than continuing the source's. By design — the source's history belongs to a volume
this clone deliberately does not share, which is the whole reason it can start at all.

`/v1` writes `SeededFrom` for the interrupted branch of `clone_base` only; the non-interrupted
clone and every other `CloneOf` are untouched. `clone_env` refuses an interrupted source outright
and so needs nothing.

Tests: `cloning_an_interrupted_source_is_allowed_and_states_the_cut_it_used` now asserts the POSTed
body carries `seededFrom` and no `cloneOf`; `a_seeded_clone_places_over_the_one_cut_it_names`
(holder admitted, holder-of-another-cut refused, no row refused);
`seed_from_snapshot_refuses_a_commit_this_node_does_not_hold` (and creates nothing when it does);
`a_seeded_clone_creates_its_own_volume_and_leaves_the_dead_owners_pin_alone`.

No RBAC or admission change. `/v1` still writes spec, the agent still writes status and creates the
child Volume as a parent's ownerReferenced child — which is exactly what the policy's create arm
already allows, whatever the source variant says. `deploy/k3s/crds.yaml` IS regenerated
(`CRD_REGEN=1`), because the variant is a new schema branch.

## F7 — the decommission notice was erased every 15 s (`493af2c5`)

The node annotation said `running=1` while the running workspace carried no `Decommissioning`
condition at all: the beat wrote it with `mark_parent`, and the owner's running-arm reconcile
rebuilds the condition list wholesale on every tick.

Fixed at the root rather than by preserving the condition through the rebuild: the parent's OWN
reconcile writes `Decommissioning=True/NodeLeaving` when its Node carries
`rustic-git.io/decommission=true`. `controller::i_am_dead` becomes `my_node` and returns
`{dead, decommissioning}` from the SAME single GET it already made, so the notice costs no extra
API call; `with_drain_notice` appends it at the running arms of `apply_workspace` (the
`PodNotReady` write and the converged write) and `apply_environment`. A stop arm never calls it, so
a stopped parent never carries it. The beat's marking loop and its test are deleted; it still
counts `running` for the drain status.

Tests: `a_running_workspace_on_a_retiring_node_carries_the_drain_notice`,
`a_running_workspace_on_an_ordinary_node_carries_no_drain_notice`,
`a_stopped_workspace_never_carries_the_drain_notice`, and
`a_drain_leaves_a_running_parent_completely_alone` (the beat writes nothing to the parent at all).

One fixture change came with it: `ctx_full` no longer appends its default Ready `node-a` when the
test supplied its own. The mock walks same-path routes in order and repeats the last, so the
default was answering "Ready and unlabelled" from the second pass onward — silently un-draining the
node midway through a multi-pass test.

## Gates (round 3)

```
cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1  → exit 0
cargo clippy --workspace --all-targets --locked -- -D warnings                   → exit 0
CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml                  → exit 0
```

## Round 3 review fixes

**Retention could sweep the cut out from under a seeded clone (`0dadee4e`).** The transient arm
deletes every other `Ready` sync point for a worktree before consulting anything, so between
`/v1`'s write and `seed_from_snapshot` the source node returning and cutting a fresh sync point was
enough to delete the pinned one — `NO_SUCH_RECORD`, which `permanent_reason` makes terminal.
`seeded_from_cuts` now reads the cuts named by any `Volume.spec.source` and the arm spares them,
keep-biased like every other listing here (a failed list deletes nothing at all this pass).

Protected until the Volume is MATERIALIZED, not for its whole life: once the bytes are copied the
clone never reads the cut again, and holding it forever would pin one extra read-only subvolume per
seeded clone on the source volume with nothing to ever release it. The `ponytail:` marker on that
arm is updated rather than dropped — it still ignores `heads` and `spec.pinned`, but it is no
longer true that nothing names a sync point by id.

Test: `a_cut_a_seeded_clone_still_needs_survives_a_newer_one` — the same newer-cut retention pass,
run twice: the cut survives while the clone's Volume is unmaterialized and is deleted once it is
Ready. `test_ctx` gained a default empty `volumes` list for the new read, appended last so a test's
own routes win.

**`seed_from_snapshot` checked before it locked (`b4ee1ba1`).** `src.exists()` ran outside
`ws_lock`, so a delete landing in that window turned a clean `NO_SUCH_RECORD` into an opaque btrfs
error the reconciler reads as transient and retries forever. Lock first, as `clone_local_ids` does;
the source voldir is created before the lock because `ws_lock`'s file lives under it, which is
`checkout`'s own opening line and creates nothing for the destination. Same commit tidies
`volume.rs`'s hand-broken `use super::{…}` list.

## Gates (round 3 review fixes)

```
cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1  → exit 0
cargo clippy --workspace --all-targets --locked -- -D warnings                   → exit 0
```

# Round 4

## F7 was written but never reached (`416616a6`)

Round 3 put the drain notice in the right place and it still did not appear on the cluster: a
converged running workspace ends its reconcile in `Action::await_change()`, so when the
decommission label landed on its Node nothing re-queued it and `with_drain_notice` never ran.
Observed on the `87a1db2b` build: annotation `running=1`, no `Decommissioning` condition on the
workspace for 3+ minutes. Removing the label was stuck the same way — a notice already written
would have outlived it forever.

The missing half was a WATCH, not a write. Each parent `Controller` (Workspace and Environment) now
also watches this node's own object, `watcher::Config::default().fields("metadata.name={me}")`,
with a mapper that turns one Node event into an `ObjectRef` for every object in that controller's
own reflector store (`all_in_store`, reading `ctl.store()` captured before the `.watches` call).
"My node changed" is not about one workspace, it is about all of them. The mapper is a sync
`FnMut` that must not do I/O; reading the store is a lock, not a request.

Two things follow for free: a readiness change reaches `my_node`'s dead-guard immediately instead
of on the next 15 s tick, and removing the label clears the notice on the spot — the running arm
rebuilds the condition list wholesale and `kept_conditions` carries only `PackagesReady`/`Attached`
forward, which is the same property that made the beat's own mark unkeepable in the first place.

The Volume controller gets no Node watch, deliberately: `apply_volume` reads the node only through
the dead-guard, which returns `requeue(TICK)` rather than `await_change()`, and nothing a Volume
writes depends on the decommission label. The decommission beat is unchanged — it still only
counts, and `mark_parent` stays deleted.

Tests: `a_node_event_maps_to_every_object_the_controller_holds` (the mapper, over a real reflector
store: two objects in, two refs out; empty store before sync enqueues nothing rather than
panicking) and `a_running_workspace_drops_a_stale_drain_notice_once_the_label_is_gone`.

That second test asserts the LAST write of the pass, and the reason is worth writing down: the
packages step's interim writes use `replaced`, which preserves every condition by type, so a stale
notice does ride along for as long as a profile build runs. The running arm below it is what clears
it and no pass ends there, so the staleness is bounded by one build rather than unbounded — but a
workspace parked in a long build will show the notice for that window.

RBAC: `nodes` gains `watch` in `deploy/k3s/agent-rbac.yaml`, table row and rule, with the reason on
the rule. A field selector narrows the stream, never the grant, so the row reads cluster-wide like
the `list` beside it and for the same reason.

## Gates (round 4)

```
cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1  → exit 0
cargo clippy --workspace --all-targets --locked -- -D warnings                   → exit 0
```

