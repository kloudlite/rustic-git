# One loop image per owner, chunk-synced to object storage — design

Date: 2026-08-31. Status: **draft for evaluation. Not approved, not scheduled.**

Consolidates a brainstorm. Where the brainstorm carried two competing shapes, this keeps the later
one (chunk + manifest) and drops the earlier one (send-stream chain with periodic fulls) — they
solve the same problem and the chain's "cut a fresh full periodically" rule exists only to bound a
replay cost the chunk design does not have.

## The design

**One loop-backed btrfs image per OWNER**, sized to that owner's allowance:

```
{pool}/img/{owner}.img          the backing file, sparse
  └─ /dev/loopN                 attached
       └─ btrfs                 mounted at {pool}/vol/{owner}/
            ├── home/live       subvolumes exactly as today
            ├── ws-{id}/live
            └── ws-{id}/live
```

Inside the image, nothing changes: the same subvolumes, the same nested cache subvolumes, the same
`live` naming.

Two independent movement paths, deliberately not fused:

**Hot path — mobility and replication: `btrfs send`/`receive`.** Unchanged from today. Moving an
owner's workspace to another node ships subvolumes incrementally. This is the latency-sensitive
path and it keeps the semantics the codebase already has.

**Cold path — durability: async chunk sync of the image to object storage.** Runs on a beat,
independent of any user-visible verb:

1. `fsfreeze -f` the filesystem INSIDE the loop device — flush and pause writes.
2. `cp --reflink=always {owner}.img snap.img` — instant CoW copy, no data moved.
3. `fsfreeze -u` — freeze window is milliseconds, not the length of the sync.
4. Walk `snap.img` in fixed 4 MiB ranges, skipping holes via `SEEK_HOLE`/`FIEMAP`, hashing each.
5. Diff against the previous manifest (`offset → content hash`).
6. Upload only chunks whose hash is not already present, keyed by hash, **scoped per owner**.
7. Write the manifest LAST. That write is the commit point.
8. Drop `snap.img`.

Restore: fetch a manifest, create a sparse file, pull chunks by offset in parallel, `losetup`,
mount. No chain, no replay.

Fixed offsets rather than content-defined chunking: CDC exists to survive insertion shifts in a
byte stream; a block image has stable offsets, so CDC costs CPU and buys nothing here.

## Positives

- **Quota by geometry.** The image is N GiB, so the filesystem inside it cannot exceed N GiB. No
  qgroups, and therefore none of their accounting cost — which scales with snapshot and extent
  count, not with volume count. This is the strongest single argument for the whole design.
- **Durability stops blocking user-visible work.** Today `push` is the one mutating verb and
  durability rides on it synchronously; a push that dies mid-flight leaves `unpushed` entries and
  nothing retries them. On the live cluster right now, `env-200e7d109a9116e4` has carried two `|u`
  layers since 2026-08-25. An idempotent background beat has no such failure shape.
- **Millisecond quiesce.** Freeze covers the reflink, not the transfer.
- **Cheap generations.** Content-keyed chunks are shared across manifests, so thirty daily
  manifests cost barely more than one.
- **Restore without replay** — strictly better than `pull_core`'s current materialize-the-lineage
  path, which fetches and replays layers in order.
- **Cross-volume consistency.** One image captures an owner's home and every workspace at a single
  instant. There is no way to get that today; each volume pushes independently.
- **Clean crash semantics.** Manifest-last means a crash yields orphan chunks — harmless and
  sweepable — never a corrupt generation. Mirrors the registry's existing
  immutable-blobs-then-record shape.
- **Sparse-aware.** A 50 GiB image holding 8 GiB reads and stores 8 GiB.
- **Block relocation is nearly free.** btrfs moves data internally (balance, defrag, CoW); because
  chunks are keyed by content, relocation changes manifest entries but uploads nothing.

## Negatives

- **btrfs-on-btrfs write amplification.** Inner CoW writes trigger outer CoW. The standard fix,
  `chattr +C` (nodatacow) on the image, fights the design: nodatacow and snapshotting defeat each
  other, since sharing extents forces CoW back on. Cheap writes or cheap outer snapshots, not both.
- **Restore granularity is the whole owner.** Recovering one 200 MiB workspace pulls the owner's
  entire image. Today a single volume's lineage restores alone. This is the sharpest regression.
- **`fsfreeze` is an operational hazard.** If the freezing process dies before thawing, the
  filesystem stays frozen and every process touching it blocks indefinitely — the agent included.
  The agent is a DaemonSet and can be OOM-killed. Needs a watchdog with a hard timeout, and the
  freezing process must never write to the frozen filesystem.
- **Loop device limits.** `max_loop` is commonly 8 by default; one per owner exceeds it quickly.
  Each attachment is also kernel state to reconcile after a reboot.
- **Fixed geometry.** Growth means resizing the image and the inner filesystem; shrinking is worse.
- **Doubled free-space accounting.** The inner filesystem can report free space while the outer
  pool is full, producing ENOSPC in confusing places.
- **No cross-owner extent sharing.** Today two workspaces from one source share extents on disk and
  ancestor blobs in the store. Separate images share nothing between owners.
- **Two candidate truths.** send/receive replicas and the object-store image can disagree. Today the
  registry is unambiguously authoritative.
- **More machinery to own:** orphan-chunk GC, manifest retention, loop attach/detach reconcile.

## Blanks

Genuinely unanswered. Each needs a decision before this could be planned.

1. **Does this REPLACE the lineage/layer model, or sit beside it?** The largest blank by far.
   `.lineage`, `recv/`, `stage/`, `LineageEntry`, `snaps/`, `layers/` and `refs/` all exist to serve
   layered push/pull. If the image is the durable unit, most of that becomes dead — but `clone`,
   `restore` and the snapshot history the UI reads (`GET /v1/volumes/{name}/history|refs`) are built
   on it. Nothing here says what happens to them.
2. **Which copy is truth after a partition?** A replica and an image can diverge. The ownership map
   exists because this question already bit this system once.
3. **qgroup overhead is unmeasured.** The quota argument — the design's strongest positive — rests
   on a number nobody has. Quotas are currently DISABLED on `/wspool-prod`, so the comparison has
   never been run.
4. **Per-workspace restore: dropped, or solved?** If dropped, say so. If solved, the mechanism is
   unspecified.
5. **Chunk size.** 4 MiB is a starting guess, not a measurement. The right value depends on inner
   write patterns nobody has profiled.
6. **Image sizing and growth policy.** Initial size, when to grow, whether shrink is ever supported.
7. **`fsfreeze` watchdog design** — timeout, recovery, and what the agent does if it finds a
   filesystem frozen at boot.
8. **Cross-region.** Each region currently holds its own copy and nothing syncs them. Unchanged
   here, or in scope?
9. **Migration.** Every existing owner has subvolumes directly on the pool. Moving them into images
   is a per-owner copy with downtime, and no path is described.
10. **Where clones fit.** `clone_local_snapshot` is a btrfs snapshot inside one filesystem. Same
    owner is fine; cloning ACROSS owners currently shares ancestor blobs and would no longer.

## Recommendation

Two of the positives do not require loop images at all and are worth separating:

- **Make durability an async reconcile** rather than a step inside `push`. This alone fixes the
  stuck-since-Aug-25 case, needs no storage change, and is small.
- **Measure qgroup overhead** on a real pool. That number decides whether the quota argument
  survives, and everything else hangs off it.

The loop-image design earns its complexity only if qgroups measure badly. Blank 1 is the one that
must be answered before any of it is plannable.
