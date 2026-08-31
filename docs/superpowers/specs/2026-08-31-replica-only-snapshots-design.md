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

## Model

**A snapshot is a read-only btrfs subvolume**, not a stream:

```
{pool}/vol/{id}/live              the working subvolume
{pool}/snap/{id}/{snapshot-id}    a read-only snapshot of it
{pool}/repl/{id}/{snapshot-id}    the same, received on a standby
```

**A `Snapshot` CR is the record.** Cluster-scoped, one per snapshot, authored by `/v1` (or by a
schedule) and reconciled by the owning node:

```yaml
spec:
  volume: ws-2ad6c7af85a3a609     # the volume it is of
  owner: karthik1729
  message: "before the refactor"  # optional, user-facing
  pinned: false                   # exempt from retention when true
status:
  phase: Ready | Working | Error
  generation: 11271               # the btrfs generation captured
  createdAt: ...
  nodes: [session-0, env-0]       # every node holding this snapshot
```

`status.nodes` is the snapshot's own `compatibleNodes` — written by whichever node holds it, using
the same guarded widen/narrow the replica reconcile already uses.

**The Workspace's status names its volume and its newest snapshot**, so one `kubectl get` answers
"what is this and how protected is it":

```yaml
status:
  volumeRef: ws-2ad6c7af85a3a609
  latestSnapshot: snap-4f2a…      # newest Ready Snapshot for this volume
  latestSnapshotAt: ...
  replicaNodes: [session-0, env-0]
```

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
| Replication is behind when a node dies | Loss window equals the beat interval — explicit, and visible as the gap between `status.generation` and the volume's live generation. |
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
