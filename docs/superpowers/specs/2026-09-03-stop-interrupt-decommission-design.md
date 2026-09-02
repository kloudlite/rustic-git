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
- The parent's status carries a `Replicated` condition, the ONE place the answer "could this
  start somewhere else" is kept, so nobody has to remember the caveats of the comparison:
  - the instant the parent starts running, the owner writes `False / Running` — from then on
    no other node is an option, whatever the copies hold;
  - after a stop, `False / AwaitingReplica` until some other node's copy holds the stop
    snapshot, then `True / Replicated` (`replicas: 1`: stays `False` with the message "no
    replica is configured for this volume").
  The owner's controller writes it on start and on every reconcile of a stopped parent (one
  field-selected VolumeReplica list). `/v1`, the web, the sweep and the start-spread chooser
  read the condition; only the owner ever computes it. A copy's own record (`branches`) is a
  fact about what it holds and is never "discarded" — the judgement lives in the condition.

### Where a stopped parent starts (spread, ruled 2026-09-03)

`may_claim` today admits a node whose replica is `Synced`. It becomes: the owner node always;
any other node only if it is **up to date** for the worktree being claimed. This is the check
that used to live in `flush_gate`; it now sits where the decision is made.

Starts spread. When a stopped parent is started and its volume is *movable* — no parent on the
volume is running — the OWNER's controller (it is alive; only the owner may give a volume away)
computes the preferred node: rendezvous over `{owner} ∪ {nodes up to date for every stopped
parent on the volume}`, keyed by the volume id (`replicate::targets`' hash, so the spread is
deterministic and even by count, and a retry lands on the same answer). If the preferred node is
the owner, it starts here as today. Otherwise the owner clears the volume's pin (`test`+
`replace`, the same CAS as the takeover), marks the volume `Released`, and un-places every parent
on it; the preferred node claims the started parent, takes the volume, and the siblings follow
on their next start. If the preferred node never claims (it died in between), the dead-node
sweep's own rule takes over: the volume is released, so any up-to-date node may claim.

A volume with a running parent is not movable, so a stopped sibling starts on the owner. A
volume with no up-to-date replica has a set of exactly `{owner}`, so it starts on the owner.
The hash is by count, not by load; weighting by free CPU or pool space is the named upgrade
(`// ponytail:` at the chooser) and needs an input every node computes identically.

### Interrupted parents

The dead-node sweep already leaves a Running parent pinned with `Degraded / NodeDead`. Two
additions:

- `/v1` answers `start` with 409 while the parent carries `NodeDead`: `"<kind> is interrupted:
  its node is down; it resumes when the node returns"`. There is no way to start it elsewhere
  and no way to abandon its edits: reaching that state is a system failure, never a workflow.
- `clone` of an interrupted parent IS allowed, as the one way forward: it grafts onto the
  newest transient any up-to-date node holds, the clone is placed on such a node, and the
  response and the web say exactly what it is based on: `"cloned from the sync point of
  14:32:07, 6 minutes before the node went down"`. The person chooses that, knowing the gap.

### Clone (ruled 2026-09-03)

Clone cuts a snapshot NOW and pokes the peers, instead of leaning on whatever the last beat left:
the owner cuts a transient (`clone-{ws}-{hex}`, same shape as a sync point) at the moment of the
request, sends `/peer/v1/wake`, and the clone is created from that cut.

Placement is the ONE rule used everywhere — the clone starts on a node that is up to date for the
source worktree, the owner always being one. There is no "same node" rule: at the instant of the
cut the owner is the only up-to-date node, so a clone of a running source lands there by
arithmetic, not by policy. A clone of a stopped or released source finds the stop transient
already replicated, so several nodes qualify and rendezvous picks among them. A clone of an
interrupted source is the one exception in kind, not in placement: it grafts onto the newest
transient an up-to-date node holds, with the age stated (see above).

`source_nodes`, which today pins a clone to the source volume's `nodeName` unconditionally, is
replaced by that same up-to-date check.

### Dead-node sweep, per volume

`release_dead_volumes` and `unclaim_kind` merge into one per-volume decision, taken on the
listing the beat already holds:

For each volume owned by a dead node:
- If any parent on it is Running → nothing moves. Every parent on the volume keeps its
  `nodeName` and carries `NodeDead`. The volume is `Unavailable / NodeDead`, pin kept.
- Else, if no other node is up to date for every parent's newest transient → nothing moves
  yet; parents carry `NodeDead` (their `Replicated=False` says which are still waiting, and the
  volume's message names them); the volume is `Unavailable`, pin kept.
- Else → the pin is cleared FIRST (`test`+`replace` as today; a lost CAS writes nothing else),
  then the volume is `Unavailable` with the empty pin, then every parent on it is un-placed so
  an up-to-date node claims it on the next start (the takeover path, unchanged).

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

Only after `drained` shows in that annotation may the VM be deleted and its flannel `/32` removed from the ZeroFS
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
| Person stops an interrupted parent | Honoured when the node returns: the cut happens then, and only then can it move. No force option (ruled): the way forward is a clone from the last synced point. |
| Clone of a parent whose volume is released (owner `""`) or whose node is dead | Today the clone is pinned to the source volume's `nodeName` and can never start. Now: see "Clone" — released or stopped sources place on an up-to-date node; an interrupted source clones from the newest replicated transient with its age stated. |
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

## Simplifications (audit of 2026-09-03)

The rules above were checked against each other and against the code for anything said twice or
kept for no reader. These are binding on the plan.

1. **One "not a place to run" predicate.** Dead (NotReady past `WS_NODE_DEAD_SECS`) and
   decommissioning (the label) are the same thing to placement. `live_nodes`, `owner_alive`,
   `may_claim` and the sweep all call one `unplaceable(node)`; nothing tests the two conditions
   separately.
2. **One "is it replicated" truth.** The `Replicated` condition is computed in one place (the
   owner: `False/Running` on start, recomputed while stopped) and read everywhere else: the sweep
   consults it instead of recomputing; the API and web show it. No separate `AwaitingReplica`
   reason on `NodeDead` — a parent waiting for both shows `NodeDead` plus `Replicated=False`.
   `replicas: 1` is not a separate reason either: `Replicated=False` with the message "no
   replica is configured for this volume".
3. **No `Released` reason.** `Unavailable` with an empty pin IS the released state; a third
   reason would restate the pin.
4. **The stop-generation annotation goes.** `stop-{parent}-{generation}` already carries the
   generation in its name; the annotation and the `STOP_GENERATION` constant are deleted.
5. **`StopPush::Landed` carries nothing.** With the flush wait gone, `unreplicated`,
   `FLUSH_TIMED_OUT`, `FlushUnreplicated` and `flush_timeout` all go; `Landed` is a unit variant.
6. **`status.compatibleNodes` is dead** once placement is the up-to-date rule: its only reader
   is the "no volume yet" arm of `may_claim`, which becomes "any placeable node". The field is
   dropped from the status writes and the CRD (kept as a tolerated-unknown on read so old
   objects still parse).
7. **One decommission annotation, not two.** `rustic-git.io/decommission-status` carries the
   whole story: `draining running=N owned=N copies=N` while in progress, `drained <RFC 3339>`
   when done. Operators grep one key.
8. **`basedOn` on every clone response**, not only the interrupted one. A clone is always based
   on a cut; the response always names the snapshot and its time, and the web always shows it.
   The interrupted case differs only in that the cut is older than "now".
9. **One per-volume decision function** for the dead-node and decommission sweeps: the three
   arms in "Dead-node sweep, per volume" are one function over the beat's listing, called with
   the dead set or the decommission set. `release_dead_volumes`, the per-kind `releasable`
   closures in `unclaim_kind`, and the `running_volumes` plumbing between them are deleted.
10. **`source_nodes` is deleted**, replaced by the same up-to-date check a start uses.
11. **Four CRD fields leave with this release**, each with zero readers afterwards:
    `Workspace/Environment.status.compatibleNodes` (item 6), `Workspace/Environment.status.durable`
    (a replica watermark nothing ever wrote; the `Replicated` condition is that answer),
    `VolumeReplica.status.lastSyncAt` (its only reader was the flush gate; up to date is by name),
    and `OwnerBinding.spec.nodeName` (the home pin, meaningless since homes moved to ZeroFS).
    Old objects still parse — the fields are simply no longer declared, written or generated.

Not simplified, on purpose: the two-step move of a volume at start (owner releases, taker
CASes) stays instead of an owner-writes-the-target handoff, because the handoff would need the
admission policy to allow any `nodeName` change, and the two-step reuses the reviewed CAS.

## What this does NOT change

- The stop snapshot itself, sync points, retention, the pull protocol, the takeover CAS, the
  admission shape for Volumes.
- The 180 s dead-node floor.
- Homes (shared NFS) are outside all of this.

## Costs, named

- Stop is seconds instead of minutes; the replica wait moves to the first cross-node start.
- One `/peer/v1/wake` POST per live node per stop and per clone.
- A start may move a volume: one CAS on the owner, one on the taker, one un-place per sibling.
- One VolumeReplica list per reconcile of a stopped parent (field-selected, cheap).
- `nodes: patch` for the agent (annotations only in practice; RBAC cannot narrow it).

## Rulings recorded (2026-09-03)

1. Starts spread across the owner and the up-to-date replica nodes, by rendezvous on the volume
   id, decided by the owner. Weighting by load is the named upgrade.
2. Decommission never stops running work. The node drains at the people's pace; an operator in
   a hurry stops workspaces through `/v1` like anyone else.
3. No "abandon edits" action exists. An interrupted parent waits for its node; the person is
   shown a clone from the last synced point, with its age, as the way forward.
4. Clone cuts a snapshot at once and wakes the peers. Placement is the same up-to-date rule as
   a start; a running source's clone lands on the owner only because nothing else is up to date
   yet — there is no same-node rule.
