# Workspaces & Environments — Design

Date: 2026-08-24 (revised 2026-08-25: storage registry, commit/push split, agent surface)
Status: approved in discussion; POC validated on Azure (see "POC results" below)

## Glossary (locked)

- **workspace** — a btrfs-backed persistent filesystem with one running container.
- **environment** — a docker-compose-like composition of services, each mounting **folders**.
- **node** — a dedicated agent VM.
- **volume** — a storage identity (the registry pointer `vol/{owner}/{id}`; one per
  workspace, one per environment).
- **folder** — an env mount unit: a directory inside the env's own volume that a service
  mounts (`model::Mount`, field `folder`; `#[serde(alias = "volume")]` for old callers).
- **push** — the one mutating verb: snapshot the live subvolume, upload the layer, register
  its record, and move the volume's registry ref, atomically, with an optional message. No
  separate commit step and no user-facing un-pushed state (the RO-snapshot-then-upload split
  survives only as an internal crash-recovery seam).
- **snapshot** — a PUSHED entry: durable in the registry, referenceable by id.
- **clone** — THE local-copy verb (`POST /v1/workspaces/{id}/clone`). Two engine paths,
  picked by the agent on whether the source's container is running: `clone_local`
  (stopped/never-pushed source, no network) and `clone_running` (live source, two-phase
  prefetch + short locked window). "fork" is gone from every user-facing surface.
- **restore** — new workspace built from an explicit past **snapshot**
  (`POST /v1/workspaces/restore`), replacing the old "from-snapshot" name.

## What this builds

A control plane and agent fleet for **workspaces** (btrfs-backed persistent filesystems with
push/clone/restore, durable as layers in region-local Azure Blob storage) and
**environments** (docker-compose-like compositions of services that mount folders),
scheduled onto VMs registered per **region**.

The snapshot mechanics were proven by a Rust POC against real Azure Blob
(`wssnap-rs`, currently on the `wssnap-bench` Azure VM): all timings and design decisions
below carry measured numbers from it.

## Decisions (settled)

- **The storage registry is a kloudlite-git namespace.** Volumes live at `vol/{owner}/{name}`
  — a third keyspace beside git repos and container images, with its own per-volume SlateDB
  routed by the existing ownership middleware (`vol` joins the reserved owner names). A
  volume's DB holds its HISTORY: commit records (lineage + state + message + timestamps),
  named refs, push status. Browsable in the web app like image tags. Layer BLOBS stay in
  the volume's region's storage account (bytes never cross regions); records reference
  blobs by id + region.
- **Git-shaped verbs.** `commit` = local RO btrfs snapshot + local lineage append (marked
  unpushed); instant, offline, no network. `push` = upload unpushed layers to the region
  blob store + write records to the volume's registry DB + move the ref. Auto-commit: the
  agent snapshots each active workspace/environment on a timer (default 5 min,
  configurable) so history exists even unasked; explicit commit any time via API; push is
  always explicit. Mutate → commit (local) → push (registry): the git correlation users
  already hold.
- **Cosmos DB keeps only the scheduling plane**: regions, agents, jobs, environments
  (composition + placement + runtime state), and slim workspace docs (scheduling identity +
  a pointer to their `vol/{owner}/{name}`). No snapshot/lineage data in Cosmos.
- **Audience-split surfaces.** `kloudlite-git-api` is the FRONTEND's door only (user CRUD,
  history browse, auth) — agents never talk to it. The agent-facing surface lives on the
  server tier next to the registry it writes: register / long-poll work / done / failed
  (handlers read-write Cosmos — any node can serve those, no routing) plus the registry
  record routes (append commits, move refs — routed to the volume's owning node). Gated by
  the per-region agent token on the public listener, the same Bearer-style pattern the OCI
  registry already uses. The scheduler and requeue sweep move to the server tier with the
  agent surface. Agents push blob bytes DIRECTLY to region storage — nothing proxies bulk
  data.
- **Data plane is per-region**: workspaces/environments run on VMs of their region; layer
  blobs live in that region's storage account. Bytes never cross regions.
- **Agents pull work via long-poll** (~30 s hold on `GET /v1/agent/work`). Outbound-only
  from VMs, works behind NAT, no connection registry; Cosmos is the record, the poll is
  only transport.
- **New crate + new binary**: `crates/workspaces` (models, scheduler, snapshot engine) and
  `bins/agent` (`kloudlite-git-agent`). The central API mounts as new routes in the existing
  `kloudlite-git-api` binary.
- **V1 scope**: control plane + workspace operations end to end. Environments are stored,
  scheduled, and started/stopped by shelling out to `docker compose`; real orchestration
  (networking, health, per-service lifecycle) is later work.
- **An environment is a composition**: services (image, command, env, mounts). An
  environment owns exactly ONE btrfs subvolume; every declared volume is a folder inside it
  (`live/volumes/{name}`), bind-mounted into services. Snapshotting an environment is one
  atomic snapshot of that single subvolume — all services' volumes captured at the same
  instant, one lineage, one push. Mounts name VOLUMES (folders), never workspaces;
  standalone workspaces are a separate feature with the same engine underneath.

## Domain model (Cosmos containers)

Partition keys chosen for the dominant query: agent work by region, snapshots by workspace.

### regions (pk: /id)
```json
{ "id": "centralindia",
  "name": "Central India",
  "storage_account": "kloudlitegitkolomi",
  "blob_container": "wslayers",
  "status": "active" }
```

### agents (pk: /region)
```json
{ "id": "agent-uuid", "region": "centralindia",
  "hostname": "vm-1", "pool": "/mnt/wspool",
  "capacity": { "cpu": 4, "mem_mb": 16384, "disk_gb": 128 },
  "used":     { "cpu": 1, "mem_mb": 2048,  "disk_gb": 30 },
  "heartbeat_at": "2026-08-24T12:00:00Z",
  "status": "alive" }
```
An agent is `alive` while `heartbeat_at` is younger than 3× the poll hold (90 s).

### workspaces (pk: /owner) — scheduling identity only after the registry reshape
```json
{ "id": "ws-uuid", "owner": "karthik", "name": "web",
  "region": "centralindia",
  "state": "ready",            // creating | ready | error | deleted
  "placement": "agent-uuid",   // null until scheduled
  "volume": "vol/karthik/web", // the registry entity holding this workspace's history
  "live_state": { "ports": [3000], "packages": ["node@22"] },  // recorded into commits
  "quota_gb": 20 }
```

### commit records — in the VOLUME's registry DB (`vol/{owner}/{name}`), NOT Cosmos

A commit persists BOTH the content (lineage of layers) and the STATE at commit time —
exposed ports, installed-package manifest (schemaless `state`). A volume created from a
commit inherits content and state together. Keyspaces in the volume DB mirror the old
snapshot/ref model: `commit/{id}` and `ref/{name}`; the single-writer owning node gives
ref moves CAS for free.

```json
{ "id": "commit-uuid",
  "state": { "ports": [3000], "packages": ["node@22"], "...": "free-form" },
  "lineage": [
    { "kind": "block",  "blob": "layer-uuid", "snap": "stream-uuid", "sha256": "..." },
    { "kind": "stream", "blob": "layer-uuid", "sha256": "..." }
  ],
  "region": "centralindia",    // where the blobs live
  "message": "before upgrade", // optional, explicit commits only
  "created_at": "..." }
```
Every record carries the FULL ordered lineage from base to itself; records reference layer
blobs only, never other records — deleting any record can never break a descendant
(POC-verified, including full-record-loss recovery). A commit exists LOCALLY first (btrfs
snapshot + local lineage entry marked unpushed); push uploads unpushed layers and writes
these records.

### environments (pk: /owner)
```json
{ "id": "env-uuid", "owner": "karthik", "name": "app-dev",
  "region": "centralindia",
  "state": "running",          // creating | running | stopped | error | deleted
  "placement": "agent-uuid",
  "ref": "snap-uuid",          // the env's OWN storage lineage (one subvolume), etag CAS
  "services": [
    { "name": "web", "image": "node:22", "command": ["npm","run","dev"],
      "env": { "PORT": "3000" },
      "mounts": [ { "folder": "appdata", "path": "/app" } ] }
  ] }
```

### jobs (pk: /region)
```json
{ "id": "job-uuid", "region": "centralindia",
  "agent": "agent-uuid",       // set by the scheduler
  "kind": "ws_create",         // ws_create | ws_push | ws_clone | ws_restore | ws_delete
                               // | env_up | env_down
  "payload": { "workspace": "ws-uuid", "...": "kind-specific" },
  "state": "leased",           // queued | leased | done | failed
  "lease_until": "...",        // leased jobs past this are requeued
  "attempts": 1,
  "error": null }
```

### CAS everywhere
Cosmos etags + conditional replace are the concurrency primitive: ref moves, job leasing,
placement writes, capacity accounting. Losing a race = read fresh, retry. No locks, no
extra generation counters.

## Layer storage (per region, unchanged from POC)

- `layers/{uuid}.zst` — layer blob. First byte is the encoding: `z` (zstd) or `r` (raw,
  chosen when a sample of the stream doesn't compress; measured: skips zstd on binaries).
- Stream layers are incremental `btrfs send -p` output; block layers are mountable btrfs
  images (mkfs `-m single -d single`, sized `used*1.5 + 1G`).
- Upload is streaming multipart: 32 MB parts, 10 in flight, hash computed in-line.
  (Single-PUT uploads die to the 180 s retry timeout on slow links — POC-measured.)
- Every lineage entry stores the blob's SHA-256; every download is verified against it
  before `btrfs receive`/mount touches it. Corrupt blob ⇒ hard error, image deleted.
- **Sidecars**: each blob gets `layers/{uuid}.json` beside it
  (`{kind, parent_blob, snap_uuid, sha256, sizes, created_at}`) so a bucket with zero
  surviving hosts and zero Cosmos data is mechanically rebuildable (`fsck` walks sidecars,
  reconstructs snapshot records and refs). The POC's record-loss incident proved rebuild
  works; sidecars remove its dependency on surviving local lineage files.
- Blobs are immutable and never deleted in v1 (billed to owner). Only records/refs/docs
  are deletable.

## Snapshot engine (crates/workspaces/src/engine/)

Direct port of the POC with its fixes:

- **Pool layout**: `{pool}/ws/{id}/live` (RW subvolume), `{pool}/recv/{stream-uuid}`
  (shared RO snapshot cache — local snapshots ARE the layer cache), `{pool}/img/*.img`
  (block images, loop-mounted as a block-restored workspace's own fs).
- **push** = the one fast verb: RO snapshot → incremental send → streaming upload →
  new snapshot record → ref CAS. Auto-squash decided at push: delta raw size >
  `squash_mb` (256 default) or stream-chain length > `chain_max` (50 default) spawns a
  background squash. A per-workspace latch prevents double-squash; the squash commit
  re-reads lineage under a file lock and grafts streams pushed during the build onto the
  new block base (POC-verified under race).
- **pull**: fetch lineage; block base restores by streamed download→decompress→loop mount
  (no per-file cost); streams apply in order via `btrfs receive`; every blob sha-verified.
  Layers already in `recv/` are never fetched.
- **clone_local** (source stopped or never pushed): new ref → same record; materialize =
  one CoW snapshot. Warm: ~20 ms, zero download. Also the fallback when the source isn't
  materialized on this pool (registry-history path).
- **clone_running** (source's container is live), two phases so the locked window is
  constant-small:
  1. prefetch everything up to the last saved snapshot onto the target (source untouched);
  2. stop the source's container (flush ⇒ clean state, not crash-state), sync, push the
     final small delta, restart source; target applies that one delta.
  POC-measured: 80 MB+300-file workspace, prefetch 1.9 s, source locked 246 ms.
  Stop/start hooks = the container/env runtime ("stop every env mounting this workspace").
  Both back the one route, `POST /v1/workspaces/{id}/clone` — the agent picks the arm by
  checking whether the source's container is running.
- **restore**: new workspace grafted onto an EXPLICIT past snapshot record (not necessarily
  the source's current tip), inheriting its lineage AND its state (ports, packages, ...).

## API (new routes in kloudlite-git-api)

User-facing (existing bearer token auth):
```
POST   /v1/regions                      admin: register region
GET    /v1/regions
POST   /v1/workspaces                   {name, region, quota_gb} → ws doc (state creating)
GET    /v1/workspaces[/{id}]
DELETE /v1/workspaces/{id}
POST   /v1/workspaces/{id}/clone        {name} → new ws cloned from this one (local-first
                                        if the source is stopped, two-phase if it's running)
POST   /v1/workspaces/restore           {name, snapshot_id, src_workspace} → new ws from
                                        an EXPLICIT snapshot record, inheriting its
                                        lineage AND its state (ports, packages, ...)
POST   /v1/environments                 {name, region, services[]} → env doc
GET    /v1/environments[/{id}]
POST   /v1/environments/{id}/start|stop
DELETE /v1/environments/{id}
```
Mutations create a job and return the doc with `state=creating/...`; readers watch state.

User-facing additions (the commit/push verbs; mutations create jobs like everything else):
```
POST /v1/workspaces/{id}/commit         {message?} → Commit job
POST /v1/workspaces/{id}/push           → Push job
POST /v1/environments/{id}/commit|push  same, against the env's volume
GET  /v1/volumes                        list the owner's volumes
GET  /v1/volumes/{name}/history         commit records, newest first
GET  /v1/volumes/{name}/refs
```

Agent-facing — on the SERVER tier, not the api (per-region token, Bearer-style gate on the
public listener; the api binary serves frontends only):
```
POST /vol-agent/register                {region, hostname, pool, capacity} → agent id
GET  /vol-agent/work?agent={id}         long-poll ≤30 s → leased job or 204 (heartbeat)
POST /vol-agent/jobs/{id}/done          {result}
POST /vol-agent/jobs/{id}/failed        {error}   → attempts+1; requeue or mark failed
POST /vol-agent/{owner}/{name}/commits  append commit records (routed to the owning node)
POST /vol-agent/{owner}/{name}/ref      move a ref (single-writer CAS)
GET  /vol-agent/{owner}/{name}/history  lineage reads for pull/clone
```
The work/jobs handlers read-write Cosmos (any node serves them); the per-volume routes go
through the ownership routing middleware exactly like repo and image routes — they join
BROWSE_TAILS' contract. The scheduler and the requeue sweep run on the server tier
alongside these handlers.

## Scheduler (in the API, runs at job creation + on requeue)

1. Candidates: agents in the target region, `alive`, with capacity for the job.
2. Prefer cache warmth: the agent already holding the workspace's layers (its current or
   previous placement), because a warm clone/pull is ~20 ms vs seconds cold.
3. Write `job.agent` + `workspace.placement` with etag CAS; loser retries.
4. Requeue sweep: leased jobs past `lease_until`, and all leased jobs of agents whose
   heartbeat aged out, go back to `queued`.

## Agent (bins/agent → kloudlite-git-agent)

Config: region, API base URL, region token, pool path, storage account creds (or SAS).
Loop: register → forever { long-poll work → execute via engine → report done/failed }.
Job execution is one at a time per workspace (engine's file lock), parallel across
workspaces. `env_up` materializes mounts (clone/pull as needed), writes a compose file from
the spec, `docker compose up -d`; `env_down` = `compose down` + final push of mounted
workspaces. Runs as root on the VM (btrfs/mount/docker); NOT in the k8s cluster.

## Testing

- Engine: unit/integration tests gated on `have_btrfs()` (loopback pools, like the POC's
  suite) — run on Linux VMs, skip elsewhere. The POC suite (15 cases: push/pull integrity,
  clone isolation, squash triggers/latch/graft, sha-in-lineage, corruption refusal,
  two-phase clone) becomes the engine's test set.
- Control plane: storage behind a small trait; tests run against an in-memory impl, CI can
  add the Cosmos emulator. Etag-CAS races covered by concurrent-writer tests.
- End-to-end: script on an Azure VM — register region+agent, create ws via API, push,
  clone, clone-under-writer, env up/down with a real compose.

## POC results (Azure, in-region, D4s_v5)

| Operation | Measured |
|---|---|
| Small-delta push (durable in Azure) | 70–150 ms |
| Push 200 files | ~106 ms |
| Full push 1.67 GB | 21.3 s (~75 MiB/s) |
| Block image upload 1.58 GB | 3.7 s (~409 MiB/s) |
| Cold block restore 2.8 GB / 300 k files | 24.3 s (mount+receive: ~22 ms) |
| Warm clone | 10–20 ms, zero download |
| Clone of running ws (80 MB) | prefetch 1.9 s, source locked 246 ms |
| Per-tiny-stream receive | ~25–29 ms (chain cap 50 is comfortable) |

## Out of scope (v1)

- Real container orchestration (networking, health checks, restarts) — compose up/down only.
- Blob GC/deletion; cross-region moves; cross-workspace dedup.
- Quota enforcement beyond image sizing; qgroups explicitly avoided.
- The k8s CSI path from the original plan — agents run on plain VMs here.

## Open questions (settle during implementation, by measurement)

1. Cosmos long-poll implementation: change feed vs. simple queued-job query loop
   (start with the query loop; it is one indexed read per second per region).
2. Ranged parallel GET for block-image restore (POC left ~3× on the table:
   24 s download vs 3.7 s upload of the same bytes).
3. Whether `env_down` should always final-push, or only on explicit save.
