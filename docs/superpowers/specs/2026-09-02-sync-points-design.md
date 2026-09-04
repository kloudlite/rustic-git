# Sync points — continuous replication of live worktrees

Date: 2026-09-02. Status: approved in discussion, ready for planning.

## Problem

Only commits replicate. A live worktree (`{pool}/vol/{volume}/live/{ws}`) is a RW subvolume on
one node; nothing watches it and nothing sends it anywhere until someone calls `push`. So:

- Lose the node and every edit since the last `push` is gone — the replicas hold the last
  commit, not the live tree.
- A workspace or environment can only be re-hosted elsewhere from its last **commit**, which may
  be hours or days behind what the person was looking at.
- The one place this was handled — the environment stop path (`stop_push`, `stop-{env}`) — cuts
  a full commit on every stop, and workspaces have no equivalent at all.

The constraint that rules out the naive fix: replicating on every tick by cutting commits would
bury the system in snapshots, CRs and retained subvolumes.

## Decision

Two kinds of snapshot, one primitive. Snapshot COUNT and replication FREQUENCY are only coupled if
every snapshot is kept; the design keeps one.

| | commit | sync point |
|---|---|---|
| made by | `push` (a person, or an environment stop) | the agent's sync beat |
| purpose | history: named, restorable, clonable | the incremental send parent, and the re-host point |
| retained | `WS_SNAPSHOT_KEEP` per chain, pinned | **exactly one per worktree** (latest) |
| in `parent` chains | yes | never — sits beside the chain, invisible in history |
| CR | `Snapshot` | `Snapshot` with `spec.transient: true` |

A sync point is a `Snapshot` CR flagged `transient`. That is the whole trick: it reuses the pull
beat, `VolumeReplica`, `btrfs send -p`, retention and the CR-first cut path unchanged, and adds one
bool, one beat, one retention rule and one checkout preference.

## Mechanism

### The sync beat (owner node)

`spawn_sync` beside `spawn_pull`, interval `WS_SYNC_SECS` (default **60**). Each pass, for every
worktree this node runs (a `Workspace`/`Environment` with `status.nodeName == me` and a pod):

1. Read the worktree's btrfs generation (`Engine::generation`, restored from history — it was
   deleted as caller-free in the shared-home work).
2. Compare with the generation recorded on the worktree's CURRENT transient (an annotation,
   `kloudlite-git.io/synced-generation`). Equal → **do nothing**. This is the old home beat's
   `homes_to_push` gate: an idle worktree costs one `btrfs subvolume show` per minute and nothing
   else.
3. Otherwise create a `Snapshot` CR `sync-{ws}-{8 hex}` with `transient: true`, `parent` = the
   PREVIOUS transient's name (so the cut is `send -p previous` — changed extents only), the
   generation annotation, and an ownerReference to the parent Workspace/Environment.

`reconcile_commit` cuts it exactly as it cuts a commit (`commit_worktree`), marks it `Ready`, and
does NOT advance `status.head` for a transient — head remains the last COMMIT, so `push`'s parent
chain is unaffected. `sync_pool` runs before the cut as it does today; that is the one per-interval
cost a busy worktree pays.

### Replication (every other node)

`pull_beat` already pulls every `Ready` `Snapshot` of every interesting volume. A transient is one
of them. `VolumeReplica.Synced` therefore means "has the latest sync point too", which is what
placement should key on. `nearest_held_ancestor` walks `spec.parent`, so the receiver applies the
incremental against the previous transient it already holds.

### Retention (every node, both directions)

Rule: **at most one Ready transient per worktree.** When a new transient reaches `Ready`, the
previous one is deleted (`retain` gains this arm; it runs on the cutting node, and `pull_volume`'s
existing "drop local commits whose CR is gone" pass drops the subvolume on every replica). A
transient is never pinned and is never a retention-protected head. Steady state per live
worktree: one extra subvolume on the owner, one per replica, regardless of uptime.

Two edge rules, both keep-biased:
- Delete the previous transient only AFTER the new one is `Ready` (never leave a worktree with
  zero sync points because a cut failed).
- If the worktree's parent object is gone, `pull_volume`'s retired-CR pass already reclaims the
  subvolume; the CR itself goes with the parent via ownerReference GC.

### Flush on close

Stop = one final sync point, gated on landing. `stop_push` becomes the sync-point cut for both
kinds (today it is environment-only and cuts a full commit): create a transient named
`stop-{ws}`/`stop-{env}` (the existing fixed-name/`STOP_GENERATION` dance is kept verbatim — it
solves the retry and stale-request problems and nothing here changes them), wait for `Ready`,
**and** wait for at least one `VolumeReplica` other than this node to report `Synced` at or after
that cut, then delete the pod. The gate on a replica is new and is the whole point: a flush that
only reached local disk is not a flush.

`WS_STOP_FLUSH_TIMEOUT_SECS` (default 600) bounds the replica wait: past it, tear down anyway and
write a `FlushUnreplicated` condition rather than parking forever — a single-node region or a
dead peer must not make stop impossible.

### Re-hosting

The checkout target on a node that has never run this worktree becomes: **the worktree's latest
Ready transient if one exists, else `status.head`** (the last commit), else the clone commit.
`effective_head` in `apply_workspace` gains that first arm; the environment path the same. The
data-loss window on node death is therefore one `WS_SYNC_SECS` of edits, bounded, instead of
"everything since the last push".

`may_claim` is unchanged: it already requires `VolumeReplica.Synced`, and Synced now includes the
transient.

## What this does NOT change

- `push`, commits, history, `clone`, `restore`: byte-for-byte the same objects and semantics. A
  commit's `parent` is still a commit; transients never enter a chain.
- Homes: on the shared NFS export, outside this entirely.
- Placement rules, replica rendezvous, the pull protocol, the peer listener.
- `WS_SNAPSHOT_KEEP` semantics for commits.

## Costs, named

- One `btrfs subvolume show` per live worktree per interval (generation read). Negligible.
- One `sync_pool` + metadata snapshot + incremental send per interval per **changed** worktree.
  Bounded by write rate, not by uptime.
- One CR create + one CR delete per interval per changed worktree. The API server does not notice.
- Stop latency: up to one replica pull cycle (`WS_REPLICA_SECS`, 300s) before the pod can go,
  because the flush waits for a replica. Acceptable for stop; if it hurts, the fix is to make the
  pull beat wake on a transient's `Ready` instead of polling, not to drop the gate.

## Cleanup folded into the same release

The survey that produced this design also found leftovers with no reader, listed in the plan's
cleanup task. They are deleted in the same release because several sit on the paths this touches
(`stop_push`, the beats, env config), and a reader meeting them alongside new code would have to
work out which are real.
