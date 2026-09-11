# Storage and placement

Every workspace tree and every environment disk is a btrfs subvolume on one node of the region, replicated to other nodes as read-only snapshots. There is no object store in the path and no network filesystem under a running tree.

## One writer, many holders

A working copy has exactly one node that runs it. Other nodes hold replicas: the newest sync point, sent as a btrfs stream between agents. A stopped working copy may start on any node whose replica is up to date; a running one stays where it is.

| Event | What moves |
|---|---|
| Edit in a running workspace | Nothing; a sync point is cut on a beat and streamed to replicas |
| Stop | A final sync point, then the pod goes; the replica catches up within seconds |
| Start | The most up-to-date node takes it; usually the same one |
| Node dies | Running copies are interrupted; stopped ones start elsewhere; every replica the node held is rebuilt on a third node |
| Node decommissioned | Running copies keep running until stopped; everything else drains ahead of time |

## Home

`/home/kl` is one directory per person per region on a shared NFS export, mounted into every workspace of yours. It is not snapshotted, not quota-bounded, and never moves; a second region has its own.

## Quota

`quota_gb` is a btrfs quota on the subvolume. Disk counted against your allocation is the sum of every working copy's quota plus every snapshot's. See [Quota](../platform/quota.md).

## Regions

A region is one Kubernetes cluster with its own nodes, home export, and nixpkgs pin. Nothing replicates across regions. See [Regions](../platform/regions.md).

## What this means for you

- A stop is seconds and never waits for a replica.
- A start after a node failure is the same bytes as the last sync point, which is at most one beat old.
- A running workspace on a dead node is a clone away: `POST /v1/workspaces/{id}/clone` grafts onto the newest replica and tells you its age.
