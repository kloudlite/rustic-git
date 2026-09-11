# Volumes

A volume is the disk under a workspace or an environment: one btrfs subvolume, its snapshots, and the nodes that hold copies. You rarely name one directly; you meet it when a working copy is gone and its snapshots are not.

## Attached and detached

| | Working copy | Snapshots | Listed as |
|---|---|---|---|
| Attached | running or stopped | 0 or more | `deleted: false` |
| Detached | deleted | 1 or more | `deleted: true` |

Deleting a working copy with snapshots detaches the volume. Deleting one without any deletes the volume too. A [restore](../workspaces/clone-and-restore.md#restore) re-attaches: the new working copy becomes an owner of the volume again.

## Delete a volume

```bash [API]
curl -sS -X DELETE https://dev.kloudlite.io/v1/volumes/$VOL -H "Authorization: Bearer $KL_TOKEN"
```

Takes a detached volume with every snapshot on it. Refused for one that still has a working copy.

## Replicas

Each volume has a replica count. Copies live on other nodes in the region as read-only snapshots, streamed between nodes. A node that dies loses nothing that had reached a replica, and its replicas are rebuilt on a third node without you doing anything.

## Disk accounting

A volume's `quota_gb` plus the size of its snapshots is what counts against your disk allocation; see [Quota](../platform/quota.md).
