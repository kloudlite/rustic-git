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
2. **Quota sums usage, with a floor.** `quota::usage` diskGb = Σ over the owner's Volumes of
   `max(status.usedBytes, FLOOR)` rounded up to GB, with `FLOOR = 1 GiB` so an empty volume is not
   free and a fresh volume whose node has not stamped yet counts 1. The builder cache counts its
   real bytes the same way (no more `BUILDER_CACHE_GB` while stopped). `spec.storage.quotaGb`
   stays the per-volume btrfs ceiling (the hard stop against one runaway volume) and is no longer a
   quota input.
3. **Creates and fills.** `guard_alloc` for disk refuses when `usage + FLOOR > limit` (a new volume
   costs its floor). When usage crosses the limit later (fills), `/v1` refuses new volumes, clone,
   restore and push (a push cuts a snapshot that can only grow the qgroup) with the same 409
   sentence, and every start of a stopped workspace, until usage is back under the limit; running
   pods keep running (their btrfs ceiling is the stop). The overview/quota views show
   `used / limit` with a per-volume `used / ceiling` bar.
4. **Wire.** `GET /v1/quota` gains `disk: {usedGb, limitGb}`; the Volume doc gains `usedBytes`,
   `usedAt`. `quota.view`/`quota.refused` probes read the new shape; a new hourly
   `vol.usage.stamped`: a workspace writes 200 MB, within two sync beats `usedBytes ≥ 200 MB`.

## Rulings folded in

- Ceilings are not reservations. A person may hold many mostly-empty volumes; the risk of everyone
  filling at once is the pool's, watched by the existing node-disk alerts, not the person's.
- No stored counter: usage is still recomputed from the CRDs on every request (the stamps are
  observed facts on the objects, refreshed by the agent), which keeps the "never cached in the api"
  invariant.
- The bench's 10 GB default ceiling (c761e721) stays as a ceiling.

## Out of scope

Per-team pooled disk; charging replicas (a replica's bytes are the platform's durability cost, not
the person's); reclaiming space automatically.
