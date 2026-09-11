# Lifecycle

Start, stop, and delete are three idempotent calls. A stop keeps everything, a delete keeps snapshots.

## Stop

Cuts a sync point, tears the pod down as soon as the cut is ready, and marks the workspace `stopped`. Seconds, and it never waits for a replica.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/stop -H "Authorization: Bearer $KL_TOKEN"
```

## Start

Brings the pod back on the same tree. The most up-to-date node takes it, which is normally the node it was on.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/start -H "Authorization: Bearer $KL_TOKEN"
```

A start is refused with `409` when the workspace is running on a node that is down: it resumes when the node returns, or you [clone](clone-and-restore.md) it.

## Delete

Removes the pod, the tree, and every sync point. Snapshots you pushed survive on a detached [volume](../snapshots/volumes.md); with none, the volume goes too.

```bash [API]
curl -sS -X DELETE https://dev.kloudlite.io/v1/workspaces/$WS -H "Authorization: Bearer $KL_TOKEN"
```

## What each keeps

| | Tree | Home | Caches | Snapshots |
|---|---|---|---|---|
| Stop | kept | kept | kept on the node | kept |
| Start on another node | newest sync point | kept | rebuilt | kept |
| Delete | gone | kept | gone | kept, on a detached volume |

## Conditions

Three conditions on the document explain anything that is not plain `ready`:

| Field | Reason | Meaning |
|---|---|---|
| `replicated` | `Running`, `AwaitingReplica`, `Replicated` | Whether another node holds the final sync point |
| `degraded` | `NodeDead` | The node is down; a running workspace is interrupted |
| `decommissioning` | `NodeLeaving` | The node is being retired; stop when convenient |
