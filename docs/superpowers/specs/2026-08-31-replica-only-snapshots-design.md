# Snapshots as CRs on replicated pools — removing object storage

Date: 2026-08-31. Status: draft for review.

Replaces the object-store durability model with local btrfs snapshots carried by node-to-node
replication, tracked as Kubernetes resources.

## Decision and what it costs

The object-store subsystem is removed outright. This is a deliberate trade, recorded here so the
next reader does not think it was an oversight:

- **Restore-to-a-past-point survives ONLY as long as a snapshot is retained on a pool.** There is
  no cold tier. Retention policy becomes load-bearing rather than a tidiness feature.
- **Cross-region ends.** Regions were joined only by blob storage; nothing replaces that here.
- **Placement narrows to nodes holding a replica** — `N` nodes, not any node. A volume can no
  longer be materialized from scratch anywhere.
- **Correlated loss is unmitigated.** Every copy lives on a pool disk in one region and one
  subscription. Deleting the resource group deletes all of them. Replication carries a `rm -rf`
  to the standbys within one beat, and an agent bug that corrupts a volume corrupts its replicas.

The gain: one storage system instead of two, snapshots that are real btrfs subvolumes rather than
replayable streams, and state visible in the API instead of in sidecar files.

## Model — a repository per volume, a worktree per workspace

A `Volume` stops being one disk with one `live` and becomes a **repository**: a commit graph plus
refs. A `Workspace` becomes a **worktree** — its own read-write subvolume, checked out at some
commit. Several workspaces derived from different points in time are several worktrees of one
repository, which is what they already are physically (btrfs extent sharing); the model now says so.

```
{pool}/vol/{volume}/snap/{commit}       read-only subvolumes — the commits
{pool}/vol/{volume}/live/{workspace}    read-write subvolumes — the worktrees
```

### Commits

A `Snapshot` CR is a commit: written once, never mutated after `Ready`, carrying its parent.

```yaml
kind: Snapshot
metadata:
  name: repo-2ad-c9f41a2b                  # {volume}-{short uuid}: see naming note below
  labels: {kloudlite.io/volume: repo-2ad, kloudlite.io/owner: karthik1729}
  ownerReferences: [{kind: Volume, name: repo-2ad, blockOwnerDeletion: true}]
spec:
  volume: repo-2ad
  parent: repo-2ad-b4e07d13                  # empty = root commit
  message: "before the refactor"
  pinned: false
status: {phase: Ready, generation: 11271, sizeBytes: 41943040}
```

`spec.parent` is both the log (walk it) and the `-p` chain a send follows.

**Naming: a uuid, not the btrfs generation.** Worktrees run on many nodes, so commits originate on
many nodes — and `Generation` is a per-filesystem counter, so two nodes can mint the same number
for different commits. The name is `{volume}-{short uuid}`; ORDER comes from `spec.parent`, never
from the name. Idempotency moves from the name to the creator: the committing agent writes the CR
first and takes the btrfs snapshot under that name second, so a retry finds the CR and continues
rather than minting a twin.

### The Volume carries no refs — they were redundant

An earlier draft kept `Volume.status.refs` (`main`, `heads/{ws}`, per-node tracking refs). Every
entry turned out to be a copy of something already owned elsewhere:

- `heads/{ws}` duplicated `Workspace.status.head` — same value, same writer (the node running that
  worktree). The scheduler is placing a Workspace, so it already holds the Workspace object;
  reading the checkout from a second place adds a copy that can skew, and nothing else needed it.
  Retention needs every branch tip, and gets it by listing Workspaces with the volume's label.
- `main` lost its meaning once worktrees commit on many nodes — there is no single mainline, only
  branch heads.
- `nodes/{name}` became `VolumeReplica`.

So the `Volume` is small: `spec` (owner, quotaGb, replicas) and a phase. The repository's content
is the `Snapshot` set; branch state lives on the Workspaces; per-node state on the VolumeReplicas.
One fact, one place, one writer.


### Per-node state lives in its own object

The repository stays declarative. What each node actually holds goes in a `VolumeReplica` — one
per (volume, node), written **only by that node**, so there is never a contended key:

```yaml
apiVersion: kloudlite.io/v1alpha1
kind: VolumeReplica
metadata:
  name: repo-2ad.session-0                  # {volume}.{node} — deterministic, idempotent
  labels: {kloudlite.io/volume: repo-2ad, kloudlite.io/node: session-0}
  ownerReferences: [{kind: Volume, name: repo-2ad, blockOwnerDeletion: true}]
spec:
  volume: repo-2ad
  node: session-0
status:
  phase: Synced                             # Synced | Syncing — nothing else
  lastSyncAt: "2026-09-01T04:10:22Z"
```

AS SHIPPED: a `branches` map (newest commit per branch this node holds) was designed for a
finer-grained scheduling arm but is NOT populated — `write_replica_status` writes phase-only, and
the claim predicate is Synced-only. The field name stays reserved; reintroduce it only together
with the scheduler arm that reads it. Only the `kloudlite.io/volume` label is stamped on a
replica row.

Dropped from earlier drafts, deliberately: `behind` (derivable from `branches` against the
Snapshot list — display sugar that can skew), `worktrees` (the same fact as
`Workspace.status.nodeName`, already owned by the claim), and a `Degraded` phase (no defined
producer or consumer; a node that cannot sync is simply `Syncing` and old, and `lastSyncAt` is
the staleness signal).

**Reaping a dead node's replica rows** — the one place the single-writer rule bends, and it must
be said explicitly: a `VolumeReplica` whose `spec.node` has not been `Ready` for
`WS_NODE_DEAD_SECS` (default 600) may be DELETED by any agent, never edited. Deletion-only keeps
the writer story clean (only the named node ever writes one), the long floor keeps a rebooting
node from being reaped mid-restart, and a reaped row is recreated by its node's next beat if the
node returns.

**Releasing a dead node's workspaces** — the same rule, applied to the claim: any agent may CLEAR
`Workspace.status.nodeName` (guarded write, never a takeover) when the named node has not been
`Ready` for the same `WS_NODE_DEAD_SECS`. The workspace is then unplaced, and the existing claim
race re-places it on a node satisfying the checkout predicate. This is the piece that has been
missing since replication landed — nothing today ever clears a claim.

`phase` is a string, not a boolean, so it can be a `selectableField` — arrays and booleans cannot
be, which is the same constraint that kept `status.nodes` off the Snapshot. Declared selectable:
`.spec.node` (each agent watches only its own) and `.status.phase` (the scheduler finds candidates
without listing everything).

### Scheduling reads it directly

To start a workspace of volume `V`, the placement step asks one indexed question:

```
VolumeReplica where spec.volume == V and status.phase == Synced
```

Any node that answers is a legal target: it holds every commit, so it can materialize a worktree
locally with one `btrfs subvolume snapshot` and no network. A node mid-catch-up is `Syncing` and
is simply not a candidate — no timeouts, no health guessing, no heuristics.

This is what makes placement dynamic. Today `status.nodeName` is written once and never cleared,
so a workspace is pinned to the node that first claimed it. With `VolumeReplica`, "where may this
run" is a query answered fresh every time, and the answer changes as nodes catch up or fall behind.

**`Synced` is a claim about commits, never about the worktree.** A node can be `Synced` and hold no
worktree at all; that is exactly the standby case, and exactly what makes it schedulable.

### Durability falls out of the same object

`durable` — the newest commit every replica holds — is `min(head)` across a volume's
`VolumeReplica` objects. Computed for display, stored nowhere, and correct by construction:
if one node is behind, the floor drops to what that node has.

A `Workspace` then surfaces two derived numbers and nothing else:

```yaml
Workspace.status:
  volumeRef: repo-2ad
  nodeName:  session-0                      # where the worktree runs now
  head:      repo-2ad-c9f41a2b                # this worktree's checkout
  durable:   repo-2ad-b4e07d13                # min(head) across replicas
```

When `head == durable` the workspace is fully protected. When they differ, the commits between
them exist on fewer nodes than they should — visible without a metric, and the same signal that
tells you replication has stalled.

### Sync is a pull, and healing is the same pull

Every commit-holding is driven by the node that WANTS the data, not the node that has it:

1. Each node's beat computes, per volume, whether it should hold that volume's commits:
   it is one of `replicate::targets(volume, ready_nodes, N)` — rendezvous over the nodes that are
   currently `Ready` — or it hosts one of the volume's worktrees.
2. It lists the volume's `Snapshot` CRs, diffs them against the subvolumes on its own pool
   (presence by name — the commit's subvolume is named after its CR), and fetches every missing
   commit oldest-first along `spec.parent`, `-p` against the parent it already holds.
3. It fetches from ANY peer whose `VolumeReplica` shows the commit — the existing peer listener,
   with the POST replaced by a GET that streams `btrfs send` on demand.

**Auto-healing is not a feature on top; it is this loop with a node missing.** A node dies; it
drops out of `Ready`; every agent's next beat recomputes rendezvous over the survivors; whichever
node is newly selected finds it holds nothing, and pulls the chain from any surviving replica. No
coordinator elects a healer, no controller notices the death — the target set changed, and the
reconcile converges on it. The dead node's `VolumeReplica` goes stale and is deleted by the same
reconcile that today removes a deselected replica.

Two nodes healing the same volume at once is harmless: both pull, both become replicas, and the
next reconcile deselects whichever rendezvous does not name — the same shape the current
replica_reconcile already handles.

**Worktrees never replicate.** Uncommitted work in a live tree exists on one node and is lost with
it — exactly git's contract. The durable floor of a workspace is its newest commit that enough
replicas hold, and nothing else.

### Where a workspace may run

The rule: **a workspace or environment can run on any node whose pool holds the commit its
worktree is checked out from.** Starting it there is one local `btrfs subvolume snapshot` of that
commit — no network, no replay.

`VolumeReplica` carries what the scheduler needs at both granularities:

```yaml
status:
  phase: Synced                # holds EVERY commit the repo currently lists
  branches:                    # newest commit of each branch this node holds
    ws-2ad6c7af85a3a609: repo-2ad-c9f41a2b
    ws-351c867c9ec91345: repo-2ad-b4e07d13
```

- The common query stays indexed and cheap: `spec.volume == V and status.phase == Synced` — a
  Synced node can host ANY of the volume's worktrees.
- The precise check honors the rule exactly: a `Syncing` node may still host workspace `w` if
  `status.branches[w]` (or an ancestor of `w`'s checkout) is present. The scheduler reads the map
  from the objects the indexed query already returned; no second round trip.

A workspace on a dead node reschedules by the same predicate: its pod is gone, its checkout commit
exists on the surviving replicas, so any Synced node is a legal restart target. What is lost is
only what was never committed.

### What a Workspace surfaces

See "Durability falls out of the same object" above — `Workspace.status` carries `nodeName`,
`head` and `durable`, all derived.

### Bootstrap: a repository with no commits yet

A brand-new volume has no commit to check out. Its first worktree is a plain
`btrfs subvolume create` (seeded by the repo-clone init container exactly as today), and the first
commit minted from it becomes the root (`spec.parent: ""`). Until that first commit lands on N
replicas the volume has no durable floor — true, and visible: `durable` is empty, the same honest
empty as an N=1 cluster.

### Homes are volumes in this model

The deletion table removes the home-push beat — which is today the ONLY durability homes have
(they were excluded from replication v1). Removing it without a replacement would leave every
owner's dotfiles, shell history and editor state with no copy anywhere. So homes join the model
as ordinary volumes: one repository per owner's home, one worktree per node the owner works on,
committed and pulled exactly like everything else. AS SHIPPED, homes carry four small
special-cases beyond `crd::home_volume_name` and the nested cache subvolumes: a Snapshot of a home
resolves its cutting node from `Volume.spec.nodeName` (no Workspace/Environment exists to route
through); the home beat cuts commits on generation movement; retention pins a home's newest
commit; and a home materializing on a node that has the CRs but not yet the bytes requeues
(`HomeAwaitingSync`) rather than bootstrapping empty.

### Verbs in this model

| verb | meaning |
|---|---|
| snapshot | commit the worktree; `main` and `heads/{ws}` advance |
| restore | in place: the RestoreWish names a commit; the agent checkout-swaps the worktree. Into a NEW workspace: `CloneOf{volume, commit}` — `VolumeSource::RestoreOf` is dead (kept deserializable only; reconciles to a Permanent re-issue condition) |
| clone | a new worktree at a named commit; no new repository, no copy |
| history | walk `spec.parent` from a ref |

### Open, needing a decision

- **Quota granularity: DECIDED — per worktree.** Each worktree subvolume carries the volume's
  `quotaGb` as its own qgroup limit. Not billing-exact for shared extents, but shared CoW extents
  do not inflate the exclusive counter, and capping runaway growth is what the limit is for
  (ruling at `ops.rs`'s set_quota).
- **Placement granularity.** `Volume.spec.nodeName` becomes plural — a repo lives on several
  nodes — and placement moves to the Workspace. That is a schema change beyond this document.

## Replication carries snapshots, not just live

Today the beat sends `live`. It now sends the volume's **retained snapshot chain**, which is both
what makes the replica useful and what makes the transfer cheap:

1. Snapshots of one volume are ordered by generation.
2. The first send to a target is full; each later one is `-p` against the previous snapshot the
   target already holds.
3. The receiver therefore accumulates the same chain, sharing extents exactly as the source does.

This is the property the object-store model provided by replaying a lineage, obtained instead by
btrfs's own extent sharing. A standby can clone from any snapshot it holds without fetching
anything.

`live` itself is no longer replicated. The newest snapshot replaces it as the recovery point,
which makes the recovery point EXPLICIT rather than "whenever the last beat happened to run".

## Retention — now load-bearing

Snapshots are the only history, and they consume pool space (deltas only, but unbounded over
time). Policy, per volume, defaulting cluster-wide:

- keep the newest `WS_SNAPSHOT_KEEP` (default 10)
- keep anything `spec.pinned`
- **never delete the snapshot `latestSnapshot` names**, even if it falls outside the keep window:
  it is the only fully-replicated recovery point, and evicting it would leave the volume with a
  history but no durable floor
- delete the rest, oldest first. AS SHIPPED the CR is deleted FIRST and each node's reconcile
  converges its disk afterwards (drop is Ok-on-absent everywhere), rather than a
  wait-for-every-node handshake — same outcome, no coordination.

Deletion is a `Snapshot` CR delete; the owning node and each replica node remove their local
subvolume on reconcile and narrow `status.nodes`. A snapshot no node holds is a `Snapshot` whose
CR is removed by its own reconciler.

## What is deleted

| file / symbol | lines | why |
|---|---|---|
| `crates/workspaces/src/engine/blob.rs` | 385 | blob upload/download/receive |
| `crates/workspaces/src/registry_client.rs` | — | the registry HTTP client |
| `bins/server/src/vol_agent.rs` | 421 | the server tier's `vol/{owner}/{id}` surface |
| `ops.rs`: `push_env`, `commit_core`, `upload_core`, `pull_core`, `restore`, `squash`, `squash_inner` | ~500 | the push/pull/squash path |
| `model.rs`: `LineageEntry`, `LayerKind`, `encode`/`parse`/`snap_name` | ~150 | the lineage encoding |
| `bins/agent`: `blob_store()`, `AZURE_*`, `WS_REGISTRY_URL`, the home-push beat | ~120 | object-store wiring |
| pool: `.lineage`, `.pushed-gen`, `stage/`, `recv/`, `img/` and their janitor sweeps | ~150 | artifacts of the old model |
| pool: `.replicated-gen-{target}` gate files and `sweep_stale_gates` | ~60 | superseded by the `nodes/{name}` refs |
| `/v1/volumes/{name}/history`, `/refs` | — | reimplemented over `Snapshot` CRs |

`SnapshotRequest` is replaced by `Snapshot`: the request and the record become one object, which
is the "everything clear" the change is for. The block-image path (`LayerKind::Block`, `img/`,
loop mounts) goes with it — it existed only to compact a blob chain.

## Verbs after the change

| verb | before | after |
|---|---|---|
| snapshot | `push` → stream to blob | create a `Snapshot` CR; the node takes a btrfs snapshot |
| restore | fetch + replay a lineage | `btrfs subvolume snapshot` from a retained snapshot |
| clone | local snapshot, else registry fallback | local snapshot only, on a node holding the source |
| history | `/v1/.../history` from the registry | `kubectl get snapshots -l volume=…`, or the same route reading CRs |

Restore and clone become local btrfs operations — milliseconds, no network, no replay. That is the
single biggest user-visible win.

## Failure modes

| failure | behaviour |
|---|---|
| A node dies holding the only copy | The volume is lost if `N=1`. `N>=2` is now a durability requirement, not a convenience — this must be documented and defaulted accordingly. |
| Pool fills with snapshots | Retention deletes oldest-first; a pool at capacity fails new snapshots loudly rather than evicting silently. |
| A snapshot exists on disk with no CR | Swept by the reconcile, same keep-biased shape as `replica_reconcile` (lookup error keeps everything). |
| A CR exists with no snapshot on any node | Reported `Error`, retained as a tombstone so the UI can say what happened rather than silently losing a row. |
| Replication is behind when a node dies | Loss window is bounded by `latestSnapshot`, which by definition is on every replica. Work after it is lost. The gap is visible as `pendingSnapshot` being set. |
| A replica target is unreachable for hours | `latestSnapshot` stops advancing while `pendingSnapshot` moves — the two fields diverging IS the alert condition, and no metric is needed to see it. |
| A user deletes their work | Replicated within one beat. Recoverable ONLY from a retained snapshot — which is why retention default and pinning matter. |
| A node dies | Its pods are gone; its `VolumeReplica`s go stale and are reaped. Rendezvous over the surviving Ready nodes selects replacements, which pull from any surviving replica. Workspaces restart on any node holding their checkout commit; uncommitted work on the dead node is lost — git's contract. |
| Two nodes heal the same volume | Both pull, both become replicas; the next reconcile deselects the one rendezvous does not name. Idempotent, no coordinator. |
| Every replica of a volume dies at once | The volume is lost. The window is the heal time (pull of the chain), which is why N and the heal loop's cadence are the two durability knobs. |

## Migration

Existing volumes carry `.lineage` files and blobs. Both become dead on the cutover:

1. Take a `Snapshot` CR for every existing volume, so nothing depends on the old history.
2. Verify each replicates to its standby.
3. Delete the old artifacts on the pool and the blobs in Azure.

Nothing reads the old format afterwards, so no compatibility shim is needed — but step 1 must be
verified complete before step 3, since step 3 is irreversible.

## Not in scope

Cross-region. Placement/failover (still nothing clears `status.nodeName`). Monitoring, which the
replication beat also lacks — but the `Snapshot` CR and the Workspace status fields are what make
it observable at all, and metrics should land with this work rather than after it.
