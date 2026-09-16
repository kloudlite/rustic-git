# Disk quota counts what is occupied, not what is reserved

Owner ruling (2026-09-17 00:25 IST): "I want quota on entire volume that actually occupied. not
reserved." Context: `diskGb: 80 of 100 in use` refused a bench create while the account held a few
GB on disk; the 80 was the sum of declared per-volume ceilings (builder cache 50 + env 20 +
workspace 10).

## Today

`quota::usage` sums `spec.storage.quotaGb` (the btrfs qgroup ceiling) over every Volume the owner
holds; the builder's `BUILDER_CACHE_GB` counts always. `guard_alloc` refuses a create when
`used + requested_ceiling > limit`. Nothing reads real bytes.

## Design

1. **The agent stamps real usage.** On every sync beat (`WS_SYNC_SECS`) and after every push/
   snapshot delete, the owning node writes `Volume.status.usedBytes: u64` = the referenced bytes of
   the volume's qgroup (the level-1 qgroup that already covers the live subvolume and its
   snapshots; `btrfs qgroup show -re --raw {pool}/vol/{id}`), plus `status.usedAt: RFC3339`. A
   node that does not hold the volume writes nothing. Status only, through the `/status`
   subresource, as every other observed fact.
2. **Disk is informational, never enforced** (owner, 2026-09-17 00:35 IST: "there is no need to be
   strict rule about the storage size. I don't want to keep checking.. it will be inefficient").
   `quota::usage` diskGb = Σ `status.usedBytes` (0 when unstamped) rounded up to GB; it is shown as
   `used / budget` everywhere quota is shown and turns amber past the budget. `guard_alloc` no longer
   has a disk dimension: no create, clone, restore, push or start is refused for disk. The only hard
   stop is each volume's btrfs ceiling (`spec.storage.quotaGb`), and the node-disk alerts.
   `BUILDER_CACHE_GB` and per-volume ceilings leave the quota math.
3. **Cost.** The stamp rides the sync beat: only when the beat already cuts a sync point for a
   worktree whose generation moved does the agent read the qgroup (one `btrfs qgroup show` on a
   path it just touched) and patch `status.usedBytes` if it changed by ≥ 1 MiB. Idle volumes cost
   nothing. No extra beat, no polling.
4. **Wire.** `GET /v1/quota` gains `disk: {usedGb, budgetGb}`; the Volume doc gains `usedBytes`,
   `usedAt`. `quota.refused` stops testing disk (it refuses on workspaces count instead);
   `quota.view` reads the new shape; new hourly `vol.usage.stamped`: a workspace writes 200 MB,
   within two sync beats `usedBytes ≥ 200 MB`.

## Rulings folded in

- Ceilings are not reservations and disk is not enforced: the risk of everyone filling at once is
  the pool's, watched by the node-disk alerts, not the person's.
- No stored counter: usage is still recomputed from the CRDs on every request (the stamps are
  observed facts on the objects, refreshed by the agent), which keeps the "never cached in the api"
  invariant.
- The bench's 10 GB default ceiling (c761e721) stays as a ceiling.

## Out of scope

Per-team pooled disk; charging replicas (a replica's bytes are the platform's durability cost, not
the person's); reclaiming space automatically.
