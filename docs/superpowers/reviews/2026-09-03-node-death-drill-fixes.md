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
