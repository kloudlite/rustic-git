# Dead nodes — replicas heal onto a third machine, volumes go Unavailable, only stopped worktrees move

Date: 2026-09-02. Status: approved in discussion, ready for planning.

## Problem

`Volume.spec.nodeName` is the owner pin: exactly one node may write to the subvolume, and the
`NodeMismatch` guard in `resolve_volume` refuses a workspace placed anywhere else. That pin is
written once, at create, and nothing ever moves it. So today:

- A node dies. `unclaim_dead_nodes` un-places its Workspaces and Environments; a survivor with a
  Synced replica claims them (`may_claim`); its `resolve_volume` then meets a Volume still owned
  by the dead node and parks the object in `Error`/`NodeMismatch` forever. The whole point of
  sync points — re-host from the latest transient — is never reached.
- Nothing tells an operator (or the API) that the volume is unusable; `kubectl get volumes`
  shows the phase it had when the node died.

The rules the owner of this repo gave: **if the node is unavailable, the volume is unavailable**;
and **never re-host a RUNNING worktree on its own** — a person who finds their workspace open on
another node minus the last minutes of edits has been hurt worse than one who finds it down. The
survivors hold the latest sync point, not the live tree; only the person can decide that the
difference is acceptable. Automatic healing is therefore limited to what has nothing to lose.

## What "heal" means here

Healing is about the COPIES, not the worktree. When a node dies, every volume it held a copy
of — as owner or as standby — is one replica short of `spec.replicas`. Healing means a third,
live machine picks up that copy from a survivor so the count is restored, and it must happen
without anyone's involvement. Moving the LIVE worktree is the separate, person-only decision
described below.

Today it does not happen. `pull_beat` computes `replicate::targets` over `pool_nodes()`, which
is every node labelled `kloudlite-git.io/pool=true` — dead or alive. The rendezvous hash keeps
pointing at the corpse, `reap_dead_replicas` deletes its row every beat, and no third node ever
finds itself a target. A cluster with three nodes and `replicas: 2` sits at one live copy until
the dead node returns.

## Decision

Four moves, all on paths that already exist.

0. **Replica placement ignores dead nodes.** `pull_beat_with` filters `pool_nodes()` through
   the same `node_is_dead` test the reaper and the unclaim sweep use (one Node list per beat,
   shared by all three), and hands `interesting_volumes` the LIVE candidates. Rendezvous then
   elects the next live node for every volume the dead one held, that node finds the volume
   "interesting", and pulls from any survivor with a Synced row — the existing source
   selection. When the OWNER is the dead node, `targets` is asked for `replicas` standbys
   instead of `replicas - 1` (the owner no longer counts as a copy), so the live copy count is
   `replicas` either way. When the node returns it is a candidate again and the hash flips back — which is the
   same event as a node JOINING, handled next.

0b. **Copies spread onto a new node and leave the node they left.** Rendezvous already gives a
   new pool node its share (about `1/N` of every volume's standby slots) the moment it carries
   the pool label: it finds those volumes interesting and pulls them. What is missing is the
   other half — the node whose slot moved keeps its subvolume and its `VolumeReplica` row
   forever, and that stale `Synced` row still lets it claim and still satisfies a stop's flush
   gate. So `pull_beat` gains a **retire pass**: for every volume this node holds locally
   (`{pool}/vol/{id}` exists) where it is not the owner, not a rendezvous target and not
   hosting a worktree, it deletes its own `VolumeReplica` row and the local subvolumes
   (`janitor::cleanup_local`). Keep-biased on purpose: retire ONLY when every current target
   already reports `Synced` — a copy is never dropped before its replacement exists, so a
   volume never has fewer live copies during a spread than it had before. A node that has
   just joined and is still pulling therefore keeps the old copy alive until it catches up;
   the old copy goes on the beat after.

1. **The dead-node sweep heals only what is stopped.** `unclaim_dead_nodes` (pull beat,
   `WS_NODE_DEAD_SECS`, default 600, same `node_is_dead` test) changes in two ways:
   - It un-places a Workspace/Environment ONLY when `spec.desiredState == Stopped`. A stopped
     worktree was flushed and replica-gated at stop time (or timed out with
     `FlushUnreplicated`, which the person already saw); moving it loses nothing new. A RUNNING
     one stays pinned to the dead node and gets condition `Degraded=True/NodeDead` with the
     message `node {n} is down; edits since the last sync point exist only there — stop and
     start to move it, or wait for the node`. Its pod is gone with the node; nothing pretends
     otherwise.
   - It gains a Volume arm: for every Volume whose `spec.nodeName` names a dead node, write
     status `phase: Unavailable`, condition `Available=False/NodeDead`, and — only when every
     parent naming that volume is `Stopped` — clear `spec.nodeName` to `""`. Clearing the
     owner is a **spec** write, the one new exception to "the agent writes status, not spec"
     beside `restoreTo`. Idempotent, runs on every node; two nodes clearing the same pin
     converge.

   **The person decides.** Stopping a workspace on a dead node (`desiredState: Stopped` through
   the existing API) is the explicit "move on": the next sweep un-places it and releases its
   volume, a survivor claims it, and starting it again re-hosts from the latest sync point that
   survivor holds. `/v1`'s stop handler answers with the loss window when the parent carries
   `NodeDead` (`"edits after {lastSyncAt} are on the dead node and will not follow"`). If the
   node returns first, its pod comes back with everything intact — the pin never moved.
2. **Takeover on claim.** A workspace or environment un-placed by the sweep is claimed as today
   (`may_claim` already requires this node's `VolumeReplica` to be `Synced` when the volume has
   commits). In `resolve_volume`, before the `NodeMismatch` guard: if the Volume's
   `spec.nodeName` is empty, this node writes itself in with a JSON-patch whose first op is
   `test /spec/nodeName == ""` — the API server makes the compare-and-set atomic, so two
   claimants cannot both win; the loser's patch fails with 422, it requeues, and on the next
   pass meets a Volume owned by the winner and refuses via the unchanged `NodeMismatch` guard
   (its own claim on the parent then stands corrected by the guard exactly as it does today
   for any other mismatch). The winner's Volume reconciler — field-selected on
   `spec.nodeName == me` — now sees the object and materializes it: the subvolume already
   exists locally (it is the replica), so `RestoreOf`/materialize is a no-op, and the
   worktree is checked out from `effective_head` (latest transient, else head).
3. **A returning node leaves it alone.** The old owner's `mine` watch is field-selected on
   `spec.nodeName`, so it never sees a volume it lost; its local subvolume keeps serving as
   an ordinary replica through the pull beat. The only leftover is the stale live worktree
   `{pool}/vol/{volume}/live/{ws}` on the old node, which nothing references. The pull beat's
   per-volume pass on a NON-owner deletes any subvolume under `live/` for that volume (they
   are unreachable by construction: a worktree only exists on the owner).

`Volume.status.phase` gains an `Unavailable` variant. Existing phases are untouched.

## Admission policy

The Volume rule in `deploy/k3s/agent-admission.yaml` allows `nodeName` to change only in the two
directions above: `oldObject.spec.nodeName == ''` (a takeover) or `object.spec.nodeName == ''`
(the sweep). An owned volume can never be re-pointed at another node in one write. What the
policy cannot check is *which* node is writing — every agent shares one ServiceAccount — so
"only the dead-node sweep clears, only a Synced node takes" is enforced in code, and the policy
enforces the shape that makes a mistaken write repairable (a cleared pin is re-taken by the
next claimant; a wrong take is refused by every other node's guard).

RBAC is already sufficient: the agent has `patch` on `volumes`.

## What this deliberately accepts

- **A partitioned-but-alive node.** A node cut off from the API server for `WS_NODE_DEAD_SECS`
  keeps its Running pods — and, under this design, keeps owning their volumes, because a
  Running worktree is never released. The only way it loses one is the person stopping it from
  the outside during the partition; on reconnect the old node sees the object is no longer its
  own, tears the pod down, and whatever was typed after the last replicated sync point is
  gone. The person asked for exactly that. Not fenced beyond this: a lease the pod must renew
  would be a second liveness system.
- **Data-loss window on an explicit stop-and-move** is one `WS_SYNC_SECS` plus one
  `WS_REPLICA_SECS` of edits — the latest transient the survivor holds. Unchanged from the
  sync-points spec, and now always chosen, never imposed.
- **A Running workspace on a dead node stays down** until the node returns or the person stops
  it. That is the price of the rule above, paid on purpose.
- **Replicas: 1.** A volume with no second replica has no Synced survivor; the sweep still
  marks it `Unavailable`, and a stopped parent waits un-placed until the node returns. On return the
  node's pull beat sees a volume with an empty owner and — because it holds the only copy —
  takes it back through the same takeover path (its replica row is Synced with itself).

## What this does NOT change

- `ensure_child_volume` still writes the pin at create from `status.nodeName`.
- `stop` on a live node: the flush gate, timeout and `FlushUnreplicated` are as before.
- The `NodeMismatch` guard for a non-empty owner. Two places naming a node still refuse rather
  than pick.
- `replicate::targets` itself, the pull protocol, source selection, sync points, commits, homes.
- Where PODS run is still first-Synced-claimant-wins; spreading owners across nodes at claim time is a separate change, not in this spec.
- `may_claim`: a healed third node has a Synced row and is therefore claimable — that is the
  point.
