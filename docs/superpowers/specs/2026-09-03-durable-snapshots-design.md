# Durable snapshots: a push outlives its workspace

**Date:** 2026-09-03
**Status:** approved by the owner, 2026-09-03 (this version supersedes the same-day drafts that used
"commit" and "pinned"; the mechanics they described are unchanged, the words are not)

## Vocabulary

What a person sees has three words. Everything else is unnamed machinery.

| word | meaning | record |
|---|---|---|
| **workspace / environment** | a working copy: a live, writable btrfs subvolume with a pod (or StatefulSets) on it | `Workspace` / `Environment` CR |
| **push** | take a **snapshot** of the working copy now, with a message | — |
| **snapshot** | a point in the working copy's life a person chose to keep. Carries its definition. Kept until explicitly deleted; the only thing that keeps a volume alive after its working copy is gone | `Snapshot` CR, `spec.transient: false` |

Machinery, named only in code and in this document:

| word | meaning | record |
|---|---|---|
| **sync point** | an automatic cut: the sync beat's (every `WS_SYNC_SECS` when the bytes OR the definition changed), a stop's, a clone's, and the migration baseline. Replication only: one kept per working copy, never listed, dies with the working copy | `Snapshot` CR, `spec.transient: true` |
| **definition** | image, packages, resources, quota and attached environment for a workspace; the services list and quota for an environment. Frozen on every cut | `Snapshot.spec.state` (`crd::SnapshotState`) |
| **volume** | the btrfs tree holding one working copy's worktrees and every cut ever taken from it; pinned to one node, replicated to `spec.replicas` nodes | `Volume` CR |
| **worktree** | the live subvolume `{pool}/vol/<volume>/live/<id>` a working copy runs on; a volume can carry several (clones, restores), all on the pin node | no CR; named by `status.volumeRef` + the parent's id |

There is no "commit", no `pinned` flag, no pin/unpin. `spec.transient` is the one distinction.

## Problem

Deleting a workspace or environment deletes everything it ever pushed: the Volume is its child,
every record is collected with the Volume, the agent removes the whole `{pool}/vol/<id>` tree and
replicas retire. A restore "when the source is gone" answers `not found` (observed live). And a
restored environment never came up at all (it never recorded its head and shared the source's live
worktree), so environment restore was broken independently.

## The rules

1. **Reference counting.** A volume is kept while a working copy or a snapshot references it, and
   collected when neither does. Nothing else counts.
2. **Deleting a working copy** deletes its worktree, its sync points, and its reference on the
   volume. Snapshots are never touched. If a snapshot remains, the volume stays (detached); if
   none does, the volume is collected as before.
3. **Ownership encodes the references.** A snapshot record is owned by the Volume; a sync point is
   owned by its working copy; the Volume is owned by each working copy that uses it. Kubernetes
   garbage collection then removes sync points with their working copy and every record with its
   Volume, and the agent's finalizer removes the working copy's owner entry from the Volume only
   when a snapshot remains (otherwise the entry stays and GC collects the Volume).
4. **A push is a snapshot.** Retention prunes sync points only (one Ready per worktree); a push is
   never pruned. The migration baseline is a sync point.
5. **Delete is the only explicit verb on a snapshot.** `DELETE /v1/volumes/{name}/snapshots/{id}`
   deletes the record; every node holding its bytes deletes the subvolume on its next beat. It
   refuses a sync point and a snapshot that is a running worktree's base. Deleting a detached
   volume's last snapshot deletes the volume. `DELETE /v1/volumes/{name}` deletes a detached
   volume with all its snapshots and refuses one that still has a working copy.
6. **Restore re-attaches.** Restoring a snapshot grafts a new working copy as a worktree of that
   volume (`CloneOf { volume, commit }` today) and makes it an owner of the Volume again. The
   snapshot's definition supplies image, packages or services; body fields override.
7. **A detached volume is a normal volume.** Same pin, same replicas, same retention. If its node
   dies the dead-node sweep clears the pin (nothing runs, nothing waits); nothing re-claims it until
   a restore, and its replicas keep the bytes.
8. **A working copy's worktree is always its own id** on whatever volume it resolves to (the
   volume id for an owned one). A restored environment runs in its own namespace on its own
   worktree; it never touches the source's live worktree.
9. **Sync on definition change, both kinds.** The sync beat cuts when the worktree's btrfs
   generation moved OR the derived definition differs from the newest Ready sync point's, so a
   package, image, resources or services change with no byte change reaches every replica within
   one beat.
10. **A delete needs the node holding the live bytes.** While that node is down the working copy
    stays Terminating. Accepted.

## Where things live

**`/v1` (`crates/workspaces/src/api.rs`)**
- push (`create_commit`): writes a snapshot (`transient: false`) owned by the Volume (controller
  ownerReference, uid from a GET; 404 if the Volume is gone).
- clone cut, and every agent cut: sync points owned by the working copy.
- `delete_snapshot`, `delete_volume`, `list_volumes`: CRD-backed (the legacy registry `upstream`
  deletes and their `registry` trait methods are removed). `list_volumes` rows carry
  `snapshots: N` and `last_push_at`. History rows list snapshots only.
- restore: unchanged shape, definition-aware (2026-09-03 snapshot-state design).

**Agent (`bins/agent`)**
- Every Workspace and Environment carries `WORKTREE_FINALIZER`; `cleanup_parent` drops the
  worktree, deletes the working copy's sync points, and detaches the Volume iff a snapshot remains
  in any phase but Error (`detach_volume`: guarded JSON patch on `metadata.ownerReferences`).
  A lost detach is an error, never a completed finalizer.
- `retire_pass` gains the byte sweep: for every volume this node holds bytes for, a `snap/<name>`
  subvolume with no record (fresh GET before each delete, keep on any error) is deleted in
  `spawn_blocking`; a stray plain directory is skipped.
- `retire_pass` also deletes a Volume with no owner entry and no snapshot, older than one beat, on
  its owner node (the crash-between-steps safety net for rule 5).
- `retain` prunes sync points only; `WS_SNAPSHOT_KEEP` is gone.
- `migrate_and_seed_baseline` writes a sync point. A baseline written by an older build
  (`transient: false`, `parent: ""`, message `"migration baseline"`) is treated as a sync point by
  `crd::Snapshot::is_snapshot()` everywhere, so no migration script is needed.
- `sync_one` cuts on generation OR definition change (rule 9).
- Admission constrains spec only, so the ownerReference patch needs no policy change; RBAC already
  grants the agent `patch` on volumes (documented). The api SA gains `snapshots: delete`,
  `volumes: delete`.

**Web**
- One Snapshots surface per kind (the environments page's archived section already exists; the
  workspace twin is added), rows from `list_volumes` with `snapshots` and `last_push_at`, each
  with Restore (the existing dialog, pre-filled from the snapshot's definition) and Delete.
- The per-workspace history page lists snapshots only.
- Delete dialogs say: "Your N snapshots stay under Snapshots; unpushed changes are deleted."
- Wording everywhere: workspace, environment, push, snapshot, restore, clone, delete.

## Existing records on the cluster

Pushes already stored are `transient: false` and become snapshots — intended. Old migration
baselines are identified by shape (above) and behave as sync points. `spec.pinned` values on
stored records are ignored and pruned by the regenerated schema.

## Cases

| case | behaviour |
|---|---|
| delete a workspace that never pushed | worktree + sync points gone, Volume collected |
| delete a workspace with 3 pushes | worktree + sync points gone; Volume detached with 3 snapshots; listed under Snapshots |
| restore from a detached volume's snapshot | new workspace as a worktree of that volume, Volume re-owned by it, definition from the snapshot |
| delete the restored workspace again | back to detached with the same snapshots plus any new pushes |
| delete a snapshot a running workspace's head names | 409 |
| delete a sync point by hand | 409 |
| delete the last snapshot of a detached volume | Volume deleted; bytes reclaimed on every node within a beat |
| delete a snapshot of an attached volume | record + bytes gone; Volume stays |
| a pushed snapshot still Working when its workspace is deleted | kept; the Volume detaches |
| node holding a detached volume dies | pin cleared; replicas keep bytes; a later restore claims on a node holding the snapshot |
| two working copies on one volume, delete one | that worktree + its sync points gone; the Volume keeps the other's owner entry |
| packages changed, no bytes changed | next sync beat cuts a sync point carrying the new definition (workspace and environment) |
| environment restore | own worktree, own namespace, head recorded, services from the snapshot (empty list means the snapshot's; none anywhere ⇒ 400) |

## Not doing

Retention windows or automatic purge of snapshots; per-owner limits (later); sharing a detached
volume across owners; an object store; keeping sync points.

## Testing

Unit and recorder tests per task in the plan; `tests/ws_e2e.sh` asserts push → delete →
`list_volumes` shows the detached volume → restore comes up with the snapshot's definition →
delete the restored working copy → delete the snapshot → Volume gone and `{pool}/vol/<id>` gone
on the node, for both kinds; then the same flow live on the cluster after deploy.
