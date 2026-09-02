# Stop, interruption and node decommission — design

Date: 2026-09-03. Status: draft for review by the owner of this repo.

## Words used here

- **Stopped**: the person (or an operator) asked for it. The pod is gone, and the last edits were
  cut into a stop snapshot on the owner node.
- **Interrupted**: the node died while the workspace or environment was running. Nobody asked.
  The live edits exist only on that node.
- **Up to date** (a replica, for a worktree): the replica HOLDS that worktree's newest Ready
  transient, by name. Each pull pass records, per worktree, the newest transient it holds in the
  replica row's `status.branches` (`worktree → snapshot name`; the field exists and is written
  empty today). Names, not clocks: `lastSyncAt` is stamped by the pulling node and `readyAt` by
  the owner, and a skewed clock must never make an old copy look current. A worktree with no
  transient at all (never ran, or a fresh restore) falls back to plain `Synced`.
- **Moving**: the right to START next time lands on another node. Nothing is ever moved while
  running, and nothing is copied from a live tree. Only flushed snapshots move.
- **Parent**: a Workspace or an Environment. **Volume**: the btrfs subvolume under one or more
  parents (a clone is a second parent on the SAME volume).

## Rules, as ruled on 2026-09-02/03

1. A running parent never moves. If its node dies it is *interrupted* and waits for the node.
   It cannot be started elsewhere and cannot be cloned until the node is back.
2. Stop flushes first, then tears down at once. Replication is asynchronous but *poked*, not
   left to the next five-minute beat.
3. A stopped parent may start on another node only once that node is up to date. Before that,
   the only place it can start is its own node.
4. Ownership is per volume, so moving is decided per volume: if any parent on a volume is
   running on the dead node, nothing on that volume moves, stopped siblings included.
5. Decommission is the planned version of node death, with one difference: whatever is running
   there KEEPS running. The node takes no new work, its copies are re-homed, and each volume is
   released as the people using it stop. Only when nothing runs there, it owns nothing, and
   every volume it touched has its full replica count elsewhere is the node `drained` and safe
   to delete.

## What changes

### Stop (both kinds)

Today `stop_push` cuts the stop transient and then WAITS for a replica to report Synced at or
after the cut (`flush_gate`, up to `WS_STOP_FLUSH_TIMEOUT_SECS`) before deleting the pod. That
wait moves out of stop and into placement.

- `stop_push` cuts `stop-{parent}-{generation}` exactly as today and, once it is Ready, tears the
  pod (or the StatefulSets) down immediately. `flush_gate`, `flush_expired`, `FlushUnreplicated`
  and `WS_STOP_FLUSH_TIMEOUT_SECS` are deleted.
- Right after the cut is Ready, the owner node POSTs `/peer/v1/wake` to every live pool node
  (peer listener, `WS_PEER_SECRET`, same NetworkPolicy as the commit route). The handler fires a
  `tokio::sync::Notify` that `spawn_pull` selects on beside its ticker, so the peers pull within
  seconds. A wake that cannot be delivered is a warn: the ticker still comes.
- The parent's status carries a `Replicated` condition on the stopped object: `False /
  AwaitingReplica` until some other node's replica is up to date for this worktree, then
  `True / Replicated`. The owner's controller writes it on every reconcile of a stopped parent
  (one field-selected VolumeReplica list, already server-side selectable on `spec.volume`).
  `/v1`'s stop and get answers include it, so the UI can say "safe to start anywhere" or "still
  copying".

### Where a stopped parent may start

`may_claim` today admits a node whose replica is `Synced`. It becomes: the owner node always;
any other node only if its replica is **up to date** for the worktree being claimed (the newest
Ready transient's `readyAt` versus the replica's `lastSyncAt`). This is the check that used to
live in `flush_gate`; it now sits where the decision is made.

While the owner node is alive nothing un-places a stopped parent, so a plain stop/start stays on
the same node as today. The replica option is taken only when the owner is dead or
decommissioned. (Load-spreading a start across replica nodes is a separate change and is not in
this spec.)

### Interrupted parents

The dead-node sweep already leaves a Running parent pinned with `Degraded / NodeDead`. Two
additions:

- `/v1` refuses `clone` (both kinds) and answers `start` with 409 while the parent carries
  `NodeDead`: `"<kind> is interrupted: its node is down; it resumes when the node returns"`.
- The web actions show that sentence.

### Dead-node sweep, per volume

`release_dead_volumes` and `unclaim_kind` merge into one per-volume decision, taken on the
listing the beat already holds:

For each volume owned by a dead node:
- If any parent on it is Running → nothing moves. Every parent on the volume keeps its
  `nodeName` and carries `NodeDead`. The volume is `Unavailable / NodeDead`, pin kept.
- Else, if no other node is up to date for every parent's newest transient → nothing moves
  yet; parents carry `NodeDead / AwaitingReplica`; the volume is `Unavailable`, pin kept.
- Else → the pin is cleared (`test`+`replace` as today), the volume is `Unavailable /
  Released`, and every parent on it is un-placed so an up-to-date node claims it on the next
  start (the takeover path, unchanged).

Today's bug — un-placing a stopped parent while a running sibling keeps the volume pinned — is
impossible under this rule because the parent is never looked at alone.

### Mismatch self-heal

`resolve_volume`'s `NodeMismatch` arm gains one branch: if the volume's owner is a live node
other than me, clear my own `status.nodeName` (guarded `replace_status`) and requeue, so the
owner reclaims it. Today the object sits in `error` forever. When the owner is dead the arm
stays as it is (refuse, wait) — the sweep above is the only thing allowed to release.

### Node decommission

Trigger: `kubectl label node <n> rustic-git.io/decommission=true`. Abort: remove the label
before `drained` is stamped.

Every node's beat treats a decommissioning node like a dead one for **placement only**: it is
not a rendezvous candidate (`live_nodes` drops it), it does not count as a copy when it owns a
volume (`standby_count(owner_alive = false)`), and it refuses claims. It keeps serving pulls,
its pod keeps running, and its copies are retired by its own retire pass only once the
replacement is Synced — exactly the existing join/leave mechanics.

The decommissioning node's own agent runs a decommission beat (30 s). It stops nothing.

1. **Running parents keep running.** They are the people's; the node waits for them to stop on
   their own. Each carries the condition `Decommissioning / NodeLeaving` with the message
   "this node is being retired; stop when convenient and the next start lands elsewhere".
   `/v1` and the web show it.
2. **Release owned volumes** as they become releasable: every parent on the volume stopped and
   some other node up to date for each. Pin cleared, `Unavailable / Decommissioned`, parents
   un-placed so their next start lands on an up-to-date node. Same code as the dead-node
   sweep's third arm.
3. **Copies settle on their own**: the node is no longer a candidate, so rendezvous re-homes
   what it held; its retire pass drops each copy once the replacement is Synced.
4. **Drained** when the node hosts no parent, owns no volume, holds no `VolumeReplica` row, and
   every volume it ever touched has `spec.replicas` Synced rows on other nodes. Stamped as the
   annotation `rustic-git.io/drained: <RFC 3339>`. Progress on the way is the annotation
   `rustic-git.io/decommission-status: "running=N owned=N awaiting=N copies=N"`, rewritten each
   beat, readable with `kubectl describe node`. This needs `patch` on `nodes` for the agent; a
   broad verb, taken knowingly, because the agent already runs as root with the host PID
   namespace on that node.

Draining therefore takes as long as the people take to stop. An operator who needs the node
sooner stops those workspaces through `/v1` like anyone else; the system never does it for them.

Only after `drained` may the VM be deleted and its flannel `/32` removed from the ZeroFS
policy. Deleting earlier is the dead-node path: copies still heal, but any volume not yet
released waits for a node that will never return.

Abort semantics: removing the label stops the beat. Parents already stopped stay stopped (start
them; they run on this node again if the volume was not yet released, elsewhere if it was).
Copies already re-homed stay; the node becomes a candidate again and rendezvous settles.

## Cases checked

Walked on 2026-09-03 against the rules above; each has an answer in this spec.

| case | answer |
|---|---|
| Stop cut done, node dies before any replica pulled it | Pod is already gone, but no node is up to date → the volume waits for the node (`NodeDead / AwaitingReplica`). Nothing moves, nothing lost. |
| Node dies between the stop request and the cut | Still running from the system's view → interrupted. Waits. |
| Person stops an interrupted parent | Honoured when the node returns: the cut happens then, and only then can it move. There is no force option; see open point 3. |
| Clone of a parent whose volume is released (owner `""`) or whose node is dead | Today the clone is pinned to the source volume's `nodeName` and can never start. Now: clone of an interrupted or decommission-pinned source is refused with 409; clone of a released volume claims like a start, on any up-to-date node for the source worktree. |
| Restore-to-new from a commit | Unchanged: a `Synced` replica holds every Ready commit, so plain `Synced` is the right bar. |
| `replicas: 1` | No standby can ever be up to date. `Replicated` stays `False / NoReplica` with a message naming it; cross-node start never offered; node death waits for the node. |
| Two stopped parents on one volume, one replicated and one not | The volume waits (every parent must be covered). Each parent's condition names its own state so the operator sees which one is holding the volume. |
| Retention deletes the sync transient before a replica pulled the stop one | The replica's newest held name is still the sync name ≠ the stop name → not up to date. Correct by construction of the name rule. |
| Two up-to-date nodes race to take a released volume | The CAS picks one; the loser's mismatch self-heal branch un-places it. |
| Start of a parent whose volume is still pinned to a decommissioning node (a sibling runs there) | The owner path applies: it starts on that node, because that is where the volume is. The node "takes no new work" only for volumes it does not own. |
| Decommissioning node dies mid-drain | The dead-node path: copies keep healing, released volumes are fine, unreleased ones wait for a node that will not return. Documented as the reason `drained` gates deletion. |
| Node NotReady for less than the 180 s floor | Nothing is marked; a start or clone issued meanwhile pends until the node is back. Acceptable. |
| Stop transient cut fails (btrfs error) | As today: no teardown, `Ready=False`, retried. Flush-first is the rule; a teardown without a cut is the one way to lose a stop. |
| Many stops in a burst | The wake `Notify` coalesces; a pull pass already running finishes and runs once more (a pending flag), never concurrently. |
| Deleting an interrupted parent | Ordinary delete; the ownerReference collects everything, and the edits on the dead node are discarded with it. The person chose to delete. |
| Environment with several services | One volume, one stop transient, same rules; the StatefulSets go down together after the cut. |

## What this does NOT change

- The stop snapshot itself, sync points, retention, the pull protocol, the takeover CAS, the
  admission shape for Volumes.
- The 180 s dead-node floor.
- Homes (shared NFS) are outside all of this.

## Costs, named

- Stop is seconds instead of minutes; the replica wait moves to the first cross-node start.
- One `/peer/v1/wake` POST per live node per stop.
- One VolumeReplica list per reconcile of a stopped parent (field-selected, cheap).
- `nodes: patch` for the agent (annotations only in practice; RBAC cannot narrow it).

## Open points for the owner

1. Start placement while the owner is alive stays on the owner node. Spreading starts across
   up-to-date replica nodes is possible later; say if you want it now.
2. Decommission never stops running work (ruled 2026-09-03). The node drains at the people's
   pace; an operator in a hurry stops workspaces through `/v1` like anyone else.
3. An interrupted parent has no "abandon my edits and start from the last sync point" action.
   A person who wants that today can only wait for the node. If you want it, it is one explicit
   `/v1` call that releases the volume from the newest replicated transient, and it must say in
   its response exactly how old that point is.
