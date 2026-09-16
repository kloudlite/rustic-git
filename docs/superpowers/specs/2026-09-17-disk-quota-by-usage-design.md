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
2. **A limit, checked at allocation moments only** (owner, 2026-09-17 00:35–00:40 IST: "I need to
   limit. but not like everytime watching whenever I write a file"). `quota::usage` diskGb =
   Σ `status.usedBytes` (0 when unstamped) rounded up to GB, plus a 1 GiB floor per volume so an
   empty volume is not free. Enforcement happens ONLY inside the verbs that already gate on quota
   — `guard_alloc` for create, clone, restore (refuse when `usage + floor > limit`) — and, when
   `usage > limit`, push, clone, restore and start-of-stopped are refused with the same
   `quota::refuse` sentence until usage is back under. No write is watched, no pod is stopped, no
   beat checks anything: the only reads are the stamps the sync beat already left. The per-volume
   btrfs ceiling (`spec.storage.quotaGb`) stays the runaway stop for one volume.
   `BUILDER_CACHE_GB` leaves the math (the builder counts its stamp).
3. **Cost.** The stamp rides the sync beat: only when the beat already cuts a sync point for a
   worktree whose generation moved does the agent read the qgroup (one `btrfs qgroup show` on a
   path it just touched) and patch `status.usedBytes` if it changed by ≥ 1 MiB. Idle volumes cost
   nothing. No extra beat, no polling.
4. **Wire.** `GET /v1/quota` gains `disk: {usedGb, budgetGb}`; the Volume doc gains `usedBytes`,
   `usedAt`. `quota.refused` sets the run owner's disk limit below its stamped usage and asserts a push 409s;
   `quota.view` reads the new shape; new hourly `vol.usage.stamped`: a workspace writes 200 MB,
   within two sync beats `usedBytes ≥ 200 MB`.

## Rulings folded in

- Ceilings are not reservations. Enforcement is at verbs, never at writes; between verbs a person
  may exceed the limit by filling, and the next verb tells them.
- No stored counter: usage is still recomputed from the CRDs on every request (the stamps are
  observed facts on the objects, refreshed by the agent), which keeps the "never cached in the api"
  invariant.
- The bench's 10 GB default ceiling (c761e721) stays as a ceiling.

## Out of scope

Per-team pooled disk; charging replicas (a replica's bytes are the platform's durability cost, not
the person's); reclaiming space automatically.
