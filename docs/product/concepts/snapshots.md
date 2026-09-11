# Snapshots

A snapshot is a read-only copy of a workspace's tree or an environment's disk at one instant. Snapshots form a chain: each names its parent, and a restore starts a new working copy from any point on it.

## Push

`push` is the one verb that makes a snapshot you keep. It takes the working copy as it is, records the object's definition with it (image, packages, quota, attached environment; for an environment, its services), and advances the head. There is no separate commit step and no un-pushed state to manage.

## Sync points

Between pushes the platform cuts sync points on its own: on a beat while the working copy changes, on every stop, and at the instant of a clone. They exist so another node always holds something recent, they are never listed as history, and they go away with the working copy. You never restore to one; a clone grafts onto one for you.

## Restore

`restore` creates a new workspace or environment from a snapshot by id. The request may override the frozen definition; anything you leave out is taken from the snapshot. A restore re-attaches the snapshot's volume, so it works even after the original working copy is gone.

An environment may also be restored in place: services drain, the disk is swapped, services come back.

## Clone

`clone` copies a running or stopped workspace into a new one right now, cutting its own sync point at the moment of the request. The response says exactly what it grafted onto (`based_on`), including the age of that cut when the source could not be cut fresh.

## Delete

Snapshots are kept until deleted. `DELETE /v1/volumes/{name}/snapshots/{id}` refuses the base of a running working copy. Deleting a detached volume's last snapshot deletes the volume.

## Next steps

::: cards
- [Push](../snapshots/push.md) — Take a snapshot, with a message.
- [History](../snapshots/history.md) — Walk the chain, find refs, restore.
- [Volumes](../snapshots/volumes.md) — What holds snapshots once the working copy is gone.
:::
