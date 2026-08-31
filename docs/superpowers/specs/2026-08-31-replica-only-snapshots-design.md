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
  name: repo-2ad-g11271                    # {volume}-g{generation}: creation is idempotent
  labels: {rustic-git.io/volume: repo-2ad, rustic-git.io/owner: karthik1729}
  ownerReferences: [{kind: Volume, name: repo-2ad, blockOwnerDeletion: true}]
spec:
  volume: repo-2ad
  parent: repo-2ad-g11200                  # empty = root commit
  message: "before the refactor"
  pinned: false
status: {phase: Ready, generation: 11271, sizeBytes: 41943040}
```

`spec.parent` is both the log (walk it) and the `-p` chain a send follows.

### Refs, on the repository

```yaml
Volume.status:
  refs:
    main:            repo-2ad-g11271       # newest commit in the repo
    nodes/session-0: repo-2ad-g11271       # what this node holds   <- origin/main
    nodes/env-0:     repo-2ad-g11200       # this replica is one behind
    heads/ws-2ad:    repo-2ad-g11271       # a worktree's checkout   <- a branch
    heads/ws-351:    repo-2ad-g11200       # another, at an older commit
  durable: repo-2ad-g11200                 # derived: oldest nodes/* ref
```

Three ref namespaces, three writers, and they never collide on a key: `main` is the owner's,
`nodes/{name}` is written only by that node, `heads/{ws}` only by the node running that worktree.

`durable` — the newest commit EVERY replica holds — is the oldest `nodes/*` ref. O(nodes) to
compute, which is why refs are pointers rather than a "who has me" list on each commit.

### What replicates: commits only

**Working trees are not replicated.** A worktree is a checkout — recreatable on any node that has
its commit, by one local `btrfs subvolume snapshot`. So replication ships the commit chain once
per repository, and every worktree derived from it comes along for free.

Three consequences, all improvements:

- The clone-ordering problem largely dissolves. Clones were expensive to replicate because each
  was a separate volume that arrived as a full copy unless sent `-c` against its ancestor. As
  worktrees of one repo they are not sent at all — only the shared commits are.
- A workspace can start on **any node holding its repository's commits**, not only where it was
  first placed. That is the placement gap, closed by the model rather than by new machinery.
- Work in a live tree since its last commit is NOT durable — exactly as in git. The window between
  `heads/{ws}` and the tree's current content is at-risk, and naming it that way sets the right
  expectation instead of implying continuous protection.

### What a Workspace surfaces

```yaml
Workspace.status:
  volumeRef:       repo-2ad
  head:            repo-2ad-g11271         # = refs.heads/{this workspace}
  latestSnapshot:  repo-2ad-g11200         # = refs.durable
  pendingSnapshot: repo-2ad-g11271         # set when head != durable
  replicaNodes:    [session-0, env-0]
```

### Verbs in this model

| verb | meaning |
|---|---|
| snapshot | commit the worktree; `main` and `heads/{ws}` advance |
| restore | move `heads/{ws}` to an older commit and re-materialize the tree — a checkout |
| clone | a new worktree at a named commit; no new repository, no copy |
| history | walk `spec.parent` from a ref |

### Open, needing a decision

- **Quota granularity.** Per repository (worktrees share extents, so this is the honest number) or
  per worktree (what users expect to be charged)? The spec assumes per repository.
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
- delete the rest, oldest first, and only once every node in `status.nodes` has dropped it

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
