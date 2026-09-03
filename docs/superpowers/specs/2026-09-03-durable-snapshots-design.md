# Durable snapshots: explicit snapshots outlive their workspace or environment

**Date:** 2026-09-03
**Status:** approved (owner, 2026-09-03: "only volumes with explicit snapshots persist; live code
goes with the workspace; explicitly taken snapshots remain independent of it"), ready for planning

## Problem

`DELETE /v1/workspaces/{id}` deletes the workspace, and with it everything it ever pushed. The
Volume is a child of the workspace (ownerReference), every pushed commit record is owned by the
Volume (since `88f2ea1e`), the agent's `cleanup_volume` removes the whole `{pool}/vol/<id>`
directory including `snap/`, replicas retire, and the orphan sweep drops the last records. A
restore "when the source is gone" — the case the snapshot-state design leads with — answers
`not found` the moment the source is deleted (observed live, 2026-09-03).

## Decision

A **commit** — a snapshot a person took with `push` — outlives the workspace or environment it was
taken from. Deleting the parent deletes its live worktree bytes and its automatic cuts (sync, stop,
clone, baseline transients), nothing else. Commits and their bytes stay on the Volume, which stays
on every node that held it, until each commit is deleted explicitly. A Volume whose last commit is
deleted deletes itself.

## Vocabulary

- **commit**: a `Snapshot` with `spec.transient == false` and phase `Ready`, taken by `push`.
- **transient**: a `Snapshot` with `spec.transient == true` (`sync-`, `stop-`, `clone-` cuts) or
  a commit that never reached `Ready`. Transients belong to their parent.
- **detached Volume**: a Volume with no parent (no ownerReference, no `Workspace`/`Environment`
  whose `status.volumeRef` names it) that holds at least one commit.

## The rules

1. **Deleting a parent never deletes a commit.** It deletes the parent's worktree subvolume, its
   transients, and the parent's ownerReference on the Volume. If the Volume then holds a commit,
   it stays, detached. If it holds none, it is deleted as today.
2. **Commit records are owned by the Volume; transient records by the parent.** Kubernetes
   garbage collection then does the right thing on either delete without the agent having to
   enumerate anything.
3. **A detached Volume is a normal Volume.** It keeps its pin, its replicas are placed and healed
   by the same rules, its bytes are swept by the same retention (which never touches commits). It
   is listed to its owner. Nothing runs on it.
4. **Restore re-attaches.** `POST /v1/workspaces/restore` / `/environments/restore` onto a commit
   of a detached Volume grafts the new parent as a worktree of that Volume (today's
   `CloneOf { volume, commit }`), and the new parent becomes the Volume's owner. The frozen state
   on the commit supplies image, packages or services (2026-09-03 snapshot-state design).
5. **Explicit delete is the only way a commit dies.** `DELETE /v1/volumes/{name}/snapshots/{id}`
   deletes one commit: the record, and the read-only subvolume on every node that holds it. It
   refuses the commit that is a live parent's `status.head` (a running worktree's base) and a
   commit some other commit's `parent` chain still needs is fine to delete (the chain is provenance,
   not storage — every commit is a full subvolume). Deleting a detached Volume's last commit
   deletes the Volume.
6. **`DELETE /v1/volumes/{name}`** deletes a detached Volume and every commit on it in one call.
   It refuses a Volume that still has a parent.

## What moves where

**Records (`/v1`, `crates/workspaces/src/api.rs`)**
- `create_commit` (push): ownerReference → the Volume (controller), not the parent.
- Clone cut, stop cut, sync cut, migration baseline: ownerReference → the parent, as today.
  The migration baseline is a commit in shape but exists to seed history; it is owned by the
  parent because a Volume that only ever had its baseline is not worth keeping. (Today's owner is
  the Volume since `54df34c9`; this flips it back.)
- `delete_ws` / `delete_env`: unchanged — they delete the parent object. Everything else is
  finalizers and GC.
- `delete_snapshot` and `delete_volume`: rewritten from the legacy registry `upstream` to the
  CRDs. `delete_snapshot`: 404 unless the caller owns the Volume; 409 `"this snapshot is the base
  of a running worktree"` when any live parent's `status.head` names it; else delete the Snapshot
  CR (the agents remove the bytes, below) and, if it was the Volume's last commit and the Volume
  is detached, delete the Volume. `delete_volume`: 409 when the Volume has a parent; else delete
  the Volume (GC takes the commits).
- `list_volumes`: already marks `deleted: true` when no live parent names the Volume; that row is
  the detached Volume, and it gains `commits: N` and `lastPushAt`.

**Agent (`bins/agent`)**
- **Parent delete** (`cleanup_workspace_worktree` for a shared clone today; extend to EVERY
  parent through one finalizer on Workspace and Environment): drop the worktree subvolume, delete
  the parent's transient Snapshot CRs (they are GC'd anyway; deleting them first keeps the
  listing honest), then decide the Volume: if `Api<Snapshot>` lists a Ready commit for it, remove
  this parent's ownerReference from the Volume (a JSON patch on `metadata.ownerReferences`, which
  the admission policy already allows for the agent's own child) and leave it; else leave the
  ownerReference so GC deletes the Volume as today.
- **Volume delete** (`cleanup_volume`): unchanged — removes the voldir. It only runs when the
  Volume is actually being deleted, which after this change means "no commit left" or an
  explicit volume delete.
- **Commit delete bytes**: a new arm in the pull beat: for every Volume this node holds
  (owner or replica), a `snap/<name>` subvolume whose Snapshot CR is gone is deleted — commits
  included. Today's retention (`snapshot.rs` `retain`) only ever deletes transients and only on
  the owner; this sweep is the byte half of rule 5 and runs everywhere the bytes are. Keep-biased:
  a Snapshot list error deletes nothing.
- **Detached Volume placement**: `interesting_volumes`, `targets`, `retire_pass` already key on
  the Volume CR and its pin, not on a parent. A detached Volume keeps its owner node; if that node
  dies, the dead-node sweep's Release arm applies (no running parent, nothing to wait for) and the
  next claim is by restore. No change expected; the plan asserts it with a test.
- **Volume with no commit and no parent**: the Volume delete arm above handles the parent-delete
  path; the last-commit-delete path is `/v1`'s. The janitor additionally deletes a Volume with no
  parent and no commit older than one janitor beat, so a crash between the two `/v1` steps cannot
  strand one.

**Web**
- The org "Snapshots" surface: `environments/page.tsx` already lists Volumes (`listVolumes`) and
  marks deleted sources. Extend that listing to workspaces too (one page per kind, or one page
  with a kind filter — match the existing environments page), show `commits` and `lastPushAt`,
  and offer **Restore** (existing dialog, pre-filled from the frozen state) and **Delete
  snapshot** / **Delete volume** with the destructive-action pattern from repo `settings/`.
- The workspace and environment delete dialogs say what stays: "Your pushed snapshots (N) stay
  under Snapshots; unpushed changes are deleted."

## Cases checked

| case | behaviour |
|---|---|
| delete a workspace that never pushed | worktree + transients gone, Volume GC'd (no commit) — today's behaviour |
| delete a workspace with 3 pushes | worktree + transients gone; Volume detached with 3 commits; listed under Snapshots |
| restore from a detached Volume's commit | new workspace as a worktree of that Volume; Volume re-owned by it; state supplies image/packages |
| delete the restored workspace again | back to detached with the same commits (plus any new pushes) |
| delete a commit that a running workspace's head names | 409 |
| delete the last commit of a detached Volume | Volume deleted, bytes reclaimed on every node |
| delete a commit of an ATTACHED Volume (parent alive) | record + bytes gone; Volume stays (it has a parent) |
| node holding a detached Volume dies | Release arm: pin cleared; a later restore's claim lands on a node that holds the commit (up-to-date rule already keys on held names) |
| two parents (clone worktrees) on one Volume, delete one | that worktree + its transients gone; Volume keeps the other parent's ownerReference — GC needs every owner gone |
| environment | identical; the "parent" is the Environment, worktree = its own id |

## Not doing

- Retention windows or automatic purge of detached Volumes: explicit delete only.
- Sharing a detached Volume across owners.
- Moving bytes to an object store; durability stays replica count.
- Keeping transients: a stop cut is not a snapshot the person took.

## Testing

- `/v1`: push → commit owned by the Volume; transient cuts owned by the parent; `delete_snapshot`
  409/404/204 and last-commit-deletes-Volume; `delete_volume` 409 with a parent; `list_volumes`
  rows carry `commits`/`lastPushAt`.
- Agent: parent finalizer drops the worktree, deletes transients, removes the ownerReference iff a
  commit exists (recorded requests); byte sweep deletes `snap/<gone>` and keeps `snap/<present>`,
  nothing on a list error; janitor deletes a parentless commitless Volume after one beat.
- Web: listing rows with counts; delete dialogs' copy; `bun test` for the pure bits.
- Live: push twice, delete the workspace, list Snapshots, restore from the first commit, confirm
  bytes and frozen state, delete the restored workspace, delete one commit, delete the last commit
  and watch the Volume and its bytes disappear on every node. Same for an environment.

## Revision 2026-09-03 (final vocabulary, owner)

The owner collapsed the vocabulary to what a person sees: **workspace / environment** (a working
copy), **push** takes a **snapshot**, kept until explicitly deleted. Everything else is unnamed
machinery. This supersedes "commit" and "pinned" above:

- A **snapshot** is any `Snapshot` record with `spec.transient == false` — exactly the pushes.
  `spec.pinned` is dropped from the CRD: it was a second flag for the same distinction.
- A **sync point** is any `spec.transient == true` record: the sync beat's, a stop's, a clone's,
  and the migration baseline (which exists only to seed replication and is never history).
- Retention prunes sync points only. `WS_SNAPSHOT_KEEP` is gone: a push is never pruned.
- Rule 1 reads: deleting a working copy deletes its worktree and its sync points; the volume stays
  iff a snapshot remains on it.
- **Sync on definition change (both kinds).** A sync point carries the definition it was cut with.
  The sync beat therefore cuts when EITHER the worktree's btrfs generation moved OR the parent's
  derived definition (`SnapshotState::of_workspace` / `of_environment`) differs from the newest
  sync point's `spec.state` — so a package or service change with no byte change still reaches
  every replica within one beat, for workspaces and environments alike.
- No pin/unpin routes. Delete is the only explicit verb on a snapshot.
