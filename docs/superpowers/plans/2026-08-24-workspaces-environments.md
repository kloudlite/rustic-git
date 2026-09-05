# Workspaces & Environments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Central Cosmos-backed control plane + per-region agents for btrfs workspaces (snapshot/fork/clone, layers in region-local Azure Blob) and compose-style environments.

**Architecture:** New `crates/workspaces` (models, store trait with mem+Cosmos impls, scheduler, snapshot engine ported from the POC) + new `bins/agent` (`kloudlite-agent`: long-polls the API, executes jobs). User+agent routes mount in the existing `kloudlite-api` binary.

**Tech Stack:** Rust workspace (axum, tokio, object_store 0.14 aws+azure, zstd zstdmt, sha2, libc), azure_data_cosmos, btrfs-progs on agent VMs.

**Spec:** `docs/superpowers/specs/2026-08-24-workspaces-environments-design.md`
**POC reference (working code, tested on Azure):** `docs/superpowers/poc/wssnap/main.rs` (+ `suite.sh`)

## Global Constraints

- Every lineage entry carries the blob's SHA-256; every download is verified before btrfs receive/mount touches it; a mismatch is a hard error and a downloaded image file is deleted.
- Layer blobs are immutable and never deleted; snapshot records carry the FULL lineage and never reference other records.
- Layer uploads are streaming multipart: 32 MB parts, 10 in flight; blob byte 0 is the encoding mode `z`|`r`.
- Push = snapshot + delta + record + ref-CAS, fast path only; squash always runs detached, guarded by the `.squashing` latch, committing under the workspace file lock with post-tip graft.
- All Cosmos writes that race use etag CAS (conditional replace); losers re-read and retry.
- Blobs stay in the workspace's region's storage account. Cosmos holds metadata only.
- `cargo clippy --workspace -- -D warnings` stays green after every task; commit subjects imperative sentence case, no tool attribution.
- btrfs-dependent tests are gated on `have_btrfs()` and skip cleanly where unavailable.

---

### Task 1: Crate scaffold, domain models, store trait with in-memory impl

**Files:**
- Create: `crates/workspaces/Cargo.toml`, `crates/workspaces/src/lib.rs`, `crates/workspaces/src/model.rs`, `crates/workspaces/src/store.rs`
- Modify: root `Cargo.toml` (members += `crates/workspaces`; `[workspace.dependencies]` += `azure_data_cosmos = "0.30"`, `async-trait = "0.1"`)

**Interfaces:**
- Produces: `model::{Region, AgentDoc, Workspace, Snapshot, LineageEntry, Environment, Service, Mount, Job, JobKind, JobState, WsState, EnvState}` (all `serde` Serialize/Deserialize, `id: String` fields as in the spec's JSON) and
  `store::MetaStore` — the trait every later task consumes.

- [ ] **Step 1: Write the models** exactly mirroring the spec JSON. Key excerpts (full field lists per spec §Domain model):

```rust
// crates/workspaces/src/model.rs
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LineageEntry {
    pub kind: LayerKind,             // Block | Stream
    pub blob: String,                // layer blob uuid
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<String>,        // Block only: contained stream-snapshot uuid
    pub sha256: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: String,
    pub region: String,
    pub agent: Option<String>,
    pub kind: JobKind,               // WsCreate WsPush WsFork WsClone WsDelete EnvUp EnvDown
    pub payload: serde_json::Value,
    pub state: JobState,             // Queued Leased Done Failed
    pub lease_until: Option<chrono::DateTime<chrono::Utc>>,
    pub attempts: u32,
    pub error: Option<String>,
}
```

- [ ] **Step 2: Write the store trait + `MemStore`.** Every method that mutates racy state takes/returns an etag:

```rust
// crates/workspaces/src/store.rs
#[async_trait::async_trait]
pub trait MetaStore: Send + Sync {
    async fn put_region(&self, r: &Region) -> Result<(), StoreErr>;
    async fn regions(&self) -> Result<Vec<Region>, StoreErr>;
    async fn upsert_agent(&self, a: &AgentDoc) -> Result<(), StoreErr>;
    async fn agents_in(&self, region: &str) -> Result<Vec<AgentDoc>, StoreErr>;
    async fn create_ws(&self, w: &Workspace) -> Result<(), StoreErr>;
    async fn get_ws(&self, owner: &str, id: &str) -> Result<Option<(Workspace, Etag)>, StoreErr>;
    async fn replace_ws(&self, w: &Workspace, etag: &Etag) -> Result<(), StoreErr>; // CasFailed on mismatch
    async fn list_ws(&self, owner: &str) -> Result<Vec<Workspace>, StoreErr>;
    async fn put_snapshot(&self, s: &Snapshot) -> Result<(), StoreErr>;
    async fn get_snapshot(&self, ws: &str, id: &str) -> Result<Option<Snapshot>, StoreErr>;
    // environments: same create/get/replace/list shape as workspaces
    async fn create_job(&self, j: &Job) -> Result<(), StoreErr>;
    async fn queued_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr>;
    async fn leased_jobs(&self, region: &str) -> Result<Vec<(Job, Etag)>, StoreErr>;
    async fn get_job(&self, region: &str, id: &str) -> Result<Option<(Job, Etag)>, StoreErr>;
    async fn replace_job(&self, j: &Job, etag: &Etag) -> Result<(), StoreErr>;
}
pub enum StoreErr { CasFailed, NotFound, Conflict, Other(String) }
```

`MemStore`: `Mutex<HashMap<..>>` per container, etag = a `u64` counter serialized to string, bumped on every replace; `replace_*` compares etags and returns `CasFailed`.

- [ ] **Step 3: Tests** in `crates/workspaces/src/store.rs` `#[cfg(test)]`: round-trip each doc type through MemStore; CAS test — two clones of a job doc, first replace wins, second gets `CasFailed`; `queued_jobs` filters by region and state.
- [ ] **Step 4:** `cargo test -p kloudlite-workspaces && cargo clippy --workspace -- -D warnings`
- [ ] **Step 5: Commit** `feat: add workspaces crate with domain models and store trait`

### Task 2: Cosmos implementation of MetaStore

**Files:**
- Create: `crates/workspaces/src/cosmos.rs`
- Modify: `crates/workspaces/src/lib.rs`

**Interfaces:**
- Consumes: `MetaStore` trait (Task 1).
- Produces: `CosmosStore::new(endpoint, key, database) -> CosmosStore` implementing `MetaStore`. Containers: `regions`(pk `/id`), `agents`(pk `/region`), `workspaces`(pk `/owner`), `snapshots`(pk `/workspace_id`), `environments`(pk `/owner`), `jobs`(pk `/region`) — created if absent at startup via `create_container_if_not_exists`.

- [ ] **Step 1:** Implement with `azure_data_cosmos` `CosmosClient::with_key`. `replace_*` passes `if_match_etag`; map the 412 response to `StoreErr::CasFailed`, 404 to `NotFound`, 409 to `Conflict`. `queued_jobs`: query `SELECT * FROM c WHERE c.state = 'queued'` with partition key = region.
- [ ] **Step 2: Tests.** Gated: `fn cosmos_env() -> Option<(String,String)>` reads `COSMOS_ENDPOINT`/`COSMOS_KEY`; tests return early (skip) when unset. Same suite as MemStore's, run against a `wstest-{uuid}` database dropped at the end.
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces` (skips cosmos without env) + clippy. Run once WITH real Cosmos env against the account the operator provides; paste results in the report.
- [ ] **Step 4: Commit** `feat: add the Cosmos MetaStore implementation`

### Task 3: Snapshot engine — pool, lineage, blob IO (POC port, part 1)

**Files:**
- Create: `crates/workspaces/src/engine/mod.rs`, `engine/pool.rs`, `engine/blob.rs`
- Test: `crates/workspaces/tests/engine_pool.rs`

**Interfaces:**
- Consumes: `docs/superpowers/poc/wssnap/main.rs` — the code to port is THERE; port verbatim then adapt names. Do not redesign.
- Produces: `engine::Pool` (POC `Pool` + `ws_lock` + `is_mountpoint`), `engine::Entry` ⇒ replaced by `model::LineageEntry` (add `encode/parse/snap_name()` helpers on it matching POC `Entry` semantics), `blob::{upload_stream, get_bytes, put_bytes, receive_into, spawn_send, sha_hex}` — signatures identical to POC minus `main()`; store handles are `Arc<dyn ObjectStore>` built by `blob::region_store(account, key, container)` (Azure) with the POC's MinIO fallback for tests via `S3_URL`.

- [ ] **Step 1:** Copy the corresponding functions from the POC file into the three modules; replace `Entry` with `LineageEntry` helpers; keep every comment that explains a WHY (multipart timeout, latch, graft).
- [ ] **Step 2:** Add `pub fn have_btrfs() -> bool` (runs `btrfs --version` + geteuid==0 check) to `engine/mod.rs`.
- [ ] **Step 3: Tests** (`engine_pool.rs`, all behind `have_btrfs()` + a loopback pool fixture that creates/mounts a 2 GB image under `tempfile` and cleans up): subvolume create + RO snapshot + `spawn_send` full stream decodes via `receive_into` into a second pool fixture; `upload_stream`→`get_bytes` round-trip against `mem://`-style local S3 (use `object_store::memory::InMemory` injected directly — no MinIO needed) verifying sha and the `z`/`r` mode byte both ways (text payload ⇒ `z`; `/dev/urandom` payload ⇒ `r`).
- [ ] **Step 4:** `cargo test -p kloudlite-workspaces --test engine_pool` on a Linux VM with btrfs (operator provides; skips locally) + clippy everywhere.
- [ ] **Step 5: Commit** `feat: port the snapshot engine pool and blob layers from the POC`

### Task 4: Snapshot engine — push, auto-squash, pull, fork, clone (POC port, part 2)

**Files:**
- Create: `crates/workspaces/src/engine/ops.rs`
- Test: `crates/workspaces/tests/engine_ops.rs`

**Interfaces:**
- Consumes: Task 3 modules; `MetaStore` for records/refs (replacing the POC's S3 `refs/`+`snaps/` objects: `Snapshot` docs + `Workspace.ref` moved by etag CAS).
- Produces:
```rust
pub struct Engine { pub pool: Pool, pub store: Arc<dyn ObjectStore>, pub meta: Arc<dyn MetaStore> }
impl Engine {
    pub async fn init(&self, ws: &Workspace) -> Result<(), EngErr>;
    pub async fn push(&self, ws: &Workspace) -> Result<PushOut, EngErr>;   // auto-squash per Global Constraints
    pub async fn pull(&self, ws: &Workspace) -> Result<(), EngErr>;
    pub async fn fork(&self, src: &Workspace, dst: &Workspace) -> Result<(), EngErr>;
    pub async fn clone_running(&self, src: &Workspace, dst: &Workspace, stop: &dyn Fn() -> Result<(),EngErr>, start: &dyn Fn() -> Result<(),EngErr>) -> Result<CloneOut, EngErr>; // two-phase per POC
    pub async fn squash(&self, ws: &Workspace) -> Result<(), EngErr>;      // called by the detached child
}
```
The detached squash child is `kloudlite-agent squash <ws-id>` (Task 7 wires the subcommand; here spawn `std::env::current_exe()` with those args exactly like the POC).

- [ ] **Step 1:** Port `push/pull/squash(+_inner graft)/fork/clone` from the POC, swapping ref/record IO to `MetaStore` (`put_snapshot` + `replace_ws` CAS for the ref move; on `CasFailed` re-read and retry once, then error). Squash thresholds from `Engine` config fields `squash_mb`/`chain_max` (env-var defaults 256/50).
- [ ] **Step 2: Tests** (btrfs-gated, `MemStore` + `InMemory` object store; this is the POC suite as Rust tests): push 200 files <1 s and drops sha-carrying entry; 7-layer cold pull into second pool byte-identical (walk + hash helper in the test file); no-op pull fetches 0; fork zero-fetch + isolation; size and chain triggers fire (thresholds 1 MB / 3 for test speed), latch blocks the second, lineage after settle = block+grafted streams and cold pull identical; corrupt a blob in the InMemory store → pull errors with sha mismatch; clone under a writer thread: locked window < 2 s, clone identical to frozen source.
- [ ] **Step 3:** Run on the btrfs VM; clippy.
- [ ] **Step 4: Commit** `feat: port push, squash, pull, fork and clone into the engine`

### Task 5: Sidecars + fsck

**Files:**
- Modify: `crates/workspaces/src/engine/blob.rs` (write `layers/{uuid}.json` after each successful layer upload: `{kind, parent_blob, snap_uuid, sha256, raw, stored, created_at}`), `engine/ops.rs` (pass parent/snap info down)
- Create: `crates/workspaces/src/engine/fsck.rs`
- Test: extend `crates/workspaces/tests/engine_ops.rs`

**Interfaces:**
- Produces: `fsck::rebuild(store, meta, region) -> Result<FsckReport, EngErr>` — lists `layers/*.json`, chains by `parent_blob`, writes one `Snapshot` doc per chain tip, returns counts; never writes refs (report only names candidate tips — a human re-points refs).

- [ ] **Step 1:** Implement sidecar write (fire after `upload_stream` returns, before the record commit — a crash between leaves an orphan blob+sidecar, which is safe).
- [ ] **Step 2: Test:** build a 5-layer lineage, wipe every `Snapshot` doc from MemStore, run `rebuild`, assert one candidate tip whose lineage matches the original, and a pull using it is byte-identical.
- [ ] **Step 3:** btrfs VM test run + clippy. **Commit** `feat: write layer sidecars and rebuild metadata with fsck`

### Task 6: User-facing API routes

**Files:**
- Create: `crates/workspaces/src/api.rs` (a `pub fn router(state) -> axum::Router` mounted by the api bin)
- Modify: `crates/api/src/lib.rs` (mount under `/v1`), `bins/api/src/main.rs` (construct `MetaStore` from env: `COSMOS_ENDPOINT/KEY/DB` else MemStore for dev)
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: `MetaStore`; existing api-tier bearer auth (`crates/api` `browse_caller` pattern — copy how existing `/v1` routes resolve the owner).
- Produces: routes exactly as in spec §API (regions admin-gated with the existing admin check; workspaces/environments owner-scoped). Every mutation writes the doc (`state: creating` etc.) + a `Job{state: Queued, region}` in that order and returns 202 with the doc.

- [ ] **Step 1:** Implement handlers; fork = new `Workspace` doc with `ref` copied from src's current ref + `WsFork` job; clone = new doc + `WsClone` job with `{src}` payload.
- [ ] **Step 2: Tests** with MemStore behind an in-process axum server (copy the shape of existing `serve_public()` in `tests/common/mod.rs`): create ws → 202, doc queued job exists; fork copies ref; unauthorized owner 403; region create requires admin.
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces --test api_user`, clippy. **Commit** `feat: add the workspaces and environments user API`

### Task 7: Agent API — register, long-poll leasing, done/failed + requeue sweep

**Files:**
- Modify: `crates/workspaces/src/api.rs` (agent route group, `PEER`-style header auth with per-region token: `regions` doc gains `agent_token` field set at region registration)
- Create: `crates/workspaces/src/lease.rs`
- Test: `crates/workspaces/tests/api_agent.rs`

**Interfaces:**
- Produces: spec §API agent routes. `GET /v1/agent/work?agent=` loops up to 30 s: `queued_jobs(region)` filtered to `job.agent == me || job.agent == None`, lease via `replace_job` CAS (`state: Leased, lease_until: now+120s`), 204 on timeout; each poll bumps `heartbeat_at` via `upsert_agent`. `lease::sweep(meta, region)`: requeue leased jobs past `lease_until` and all leased jobs of agents with `heartbeat_at` older than 90 s (attempts+1; `attempts > 3` ⇒ `Failed`). Sweep runs in the API on a 30 s `tokio::spawn` interval per known region.

- [ ] **Step 1:** Implement; poll interval inside the long-poll loop 1 s.
- [ ] **Step 2: Tests:** register agent; queued job returned to poller and CAS-leased exactly once with two concurrent pollers (spawn both, assert one job, one 204); done marks Done; failed with attempts=4 marks Failed; sweep requeues an expired lease and a dead agent's job.
- [ ] **Step 3:** test + clippy. **Commit** `feat: add agent registration, work leasing and the requeue sweep`

### Task 8: Scheduler

**Files:**
- Create: `crates/workspaces/src/scheduler.rs`
- Modify: `crates/workspaces/src/api.rs` (call `schedule` after job creation; also from the sweep for requeued jobs)
- Test: `crates/workspaces/tests/scheduler.rs`

**Interfaces:**
- Produces: `schedule(meta, job) -> Result<Option<String /*agent id*/>, StoreErr>`: candidates = alive agents in `job.region` with free capacity (`capacity - used` fits a fixed per-job reservation: ws jobs 1 cpu/1 GB, env jobs the sum of service reservations, default 1 cpu/512 MB per service); prefer the agent equal to the workspace's current `placement`, else most free disk; write `job.agent` (etag CAS) then `workspace.placement`; on CAS loss re-read and retry twice.

- [ ] **Step 1:** Implement + unit tests with MemStore: warm placement preferred; capacity excludes a full agent; dead agent excluded; no candidates leaves the job queued (returns None, job stays Queued).
- [ ] **Step 2:** test + clippy. **Commit** `feat: add the warmth-aware region scheduler`

### Task 9: The agent binary

**Files:**
- Create: `bins/agent/Cargo.toml` (bin name `kloudlite-agent`), `bins/agent/src/main.rs`
- Modify: root `Cargo.toml` (members + default-members += `bins/agent`), `Dockerfile` is NOT touched (agents run on VMs, installed by script)

**Interfaces:**
- Consumes: `Engine` (Task 4), agent HTTP API (Task 7) via `reqwest`.
- Produces: `kloudlite-agent run` with env config `WS_API_URL, WS_REGION, WS_AGENT_TOKEN, WS_POOL, AZURE_ACCOUNT/KEY/CONTAINER`; also the hidden `kloudlite-agent squash <ws-id>` subcommand the engine's detached spawn uses (constructs Engine from the same env and calls `squash`).

- [ ] **Step 1:** Implement: register (id persisted at `{pool}/agent-id`), then loop `GET work` → match kind → engine call (`WsCreate`→init, `WsFork`→fork, `WsClone`→clone_running with stop/start = `docker compose stop/start` of envs mounting the src (v1: payload carries the compose project names; empty list ⇒ hooks are no-ops), `WsDelete`→subvolume delete + doc update via `done` payload, `WsPush`→push, `EnvUp`/`EnvDown`→Task 10 stubs returning Failed until then) → `POST done|failed`. One tokio task per job, per-workspace serialization already enforced by the engine's flock.
- [ ] **Step 2: Test:** integration test `bins/agent/tests/loop.rs` gated on `have_btrfs()`: in-process axum app (MemStore) + real agent loop as a spawned task + a `WsCreate` then `WsPush` job; assert docs reach `ready` and a snapshot record exists.
- [ ] **Step 3:** btrfs VM run + clippy. **Commit** `feat: add the kloudlite-agent binary`

### Task 10: Environments — compose up/down

**Files:**
- Create: `crates/workspaces/src/engine/compose.rs`
- Modify: `bins/agent/src/main.rs` (EnvUp/EnvDown arms)
- Test: extend `bins/agent/tests/loop.rs`

**Interfaces:**
- Produces: `compose::up(env: &Environment, mounts: &[(String /*ws id*/, PathBuf /*live dir*/)], dir: &Path) -> Result<(), EngErr>` — renders a `docker-compose.yml` from the spec's services (image, command, environment, one bind volume per mount to its declared `path`), runs `docker compose -p env-{id} up -d`; `down(env, dir)` runs `compose -p env-{id} down` then final-pushes every mounted workspace (spec open question 3 resolved: always push on down — safest default, revisit on cost).

- [ ] **Step 1:** Implement render (serde_yaml) + up/down. EnvUp job flow in the agent: for each mount, workspace present locally? else `pull`; then `up`; report done → API sets `state: running`.
- [ ] **Step 2: Test** (gated on `have_btrfs()` AND docker present): env with one `alpine:3` service `sh -c 'echo hi > /ws/out.txt; sleep 300'` mounting a workspace; after up, `out.txt` exists in the live subvolume; down pushes (snapshot count grew) and the container is gone.
- [ ] **Step 3:** VM run + clippy. **Commit** `feat: render and run environment compositions with docker compose`

### Task 11: End-to-end script + docs

**Files:**
- Create: `tests/ws_e2e.sh` (chmod +x)
- Modify: `CLAUDE.md` (one Commands line + one architecture paragraph), `README.md` (short section pointing at the spec)

- [ ] **Step 1:** Script (mirrors `registry_e2e.sh` conventions, exit 77 when prerequisites absent): needs btrfs+docker+`COSMOS_*`+`AZURE_*` env; starts the api bin with CosmosStore, starts an agent, registers a region via API, then: create ws → wait ready → write files into live → push job → fork → clone-under-writer (background writer, stop hook via compose) → env up with the ws → verify file written by the service → env down → asserts at each step, cleanup deletes the Cosmos test db and Azure blobs under a test prefix.
- [ ] **Step 2:** Run it on the Azure VM (`wssnap-bench` has btrfs/docker; operator supplies Cosmos endpoint+key). Paste output in the report.
- [ ] **Step 3:** Update CLAUDE.md/README. **Commit** `feat: add the workspaces end-to-end test and docs`

---

## Execution notes

- Tasks 3, 4, 5, 9, 10, 11 need a Linux VM with btrfs + root; everything else runs anywhere. The operator provides the VM and Cosmos credentials when those tasks reach testing.
- The POC file `docs/superpowers/poc/wssnap/main.rs` is working, Azure-tested code — porting tasks copy from it and adapt, never rewrite from prose.

---

# Reshape extension (2026-08-25): storage registry, commit/push split, agent surface

Spec revision authority: the same spec doc, revised sections "Decisions", "commit records",
"API". Global constraints unchanged and still binding (sha verification, CAS, immutable
blobs, WHY-comments, clippy green).

### Task 13: The vol/ registry namespace on the server tier

**Files:**
- Create: `crates/workspaces/src/registry.rs` (volume DB keyspaces + record types over the
  storage crate's per-entity SlateDB pattern — study `crates/storage/src/pool.rs` lease API
  and how `crates/registry/src/store.rs` opens per-image DBs; copy that shape for
  `repo/vol/{owner}/{name}`)
- Modify: `crates/storage/src/store.rs` or wherever RESERVED_REPO_NAMES lives (add `vol`),
  `bins/server/src/router/route.rs` (routing key `vol/{owner}/{name}` for the new tails;
  extend BROWSE_TAILS so `every_browse_route_is_routable` holds), `bins/server/src/` new
  module `vol_agent.rs` mounting the per-volume record routes:
  POST /vol-agent/{owner}/{name}/commits (append commit records), POST .../ref (move ref,
  single-writer CAS), GET .../history (lineage reads). Auth: per-region agent token —
  region docs are in Cosmos, so the server verifies via a shared-secret env fallback
  KLOUDLITE_VOL_AGENT_TOKENS (comma list) in this task; Cosmos-backed lookup arrives with
  Task 14's Cosmos client. Constant-time compare.
- Test: `bins/server/tests/vol_agent.rs` — in-process server (mem store): append commits to
  vol/alice/web, move ref, read history back; routing test: the new tails appear in
  BROWSE_TAILS and route; reserved-name test: a user cannot claim owner `vol`.

**Step ladder:** failing test → impl → green → clippy → commit `Add the vol registry
namespace with agent record routes`.

### Task 14: Agent work surface moves to the server tier

**Files:**
- Modify: `bins/server/src/vol_agent.rs` (+ register/work/done/failed/scheduler/sweep),
  `bins/server/src/boot.rs` or main (construct the Cosmos-backed MetaStore when
  COSMOS_ENDPOINT set; spawn the sweep), root Cargo.toml (server depends on
  kloudlite-workspaces), `crates/workspaces/src/api.rs` (DELETE the /v1/agent routes +
  their tests move), `bins/api/src/main.rs` (drop the sweep spawn).
- The handlers are the Task 7/8 code MOVED, not rewritten: register, long-poll lease (CAS),
  done/failed with ws/env state transitions, scheduler, lease sweep. Region agent_token
  auth now reads the region doc from Cosmos (replacing Task 13's env fallback, which stays
  as a break-glass override).
- Test: move/adapt api_agent.rs tests to `bins/server/tests/vol_agent.rs` (MemStore
  in-process); api_user tests updated (agent routes now 404 on the api).

Commit: `Move the agent work surface to the server tier`.

### Task 15: Engine commit/push split + agent rewire

**Files:**
- Modify: `crates/workspaces/src/engine/ops.rs` — split `push` into `commit` (RO snapshot +
  local lineage append marked unpushed: lineage file entries gain a `!` suffix or a
  sidecar `.unpushed` list — pick one, document WHY) and `push` (upload unpushed layers →
  POST records to the vol-agent routes → move ref → clear marks). Auto-squash decision
  moves to push (only pushed layers squash). `pull`/`fork`/`clone` read history via GET
  .../history. The MetaStore snapshot/ref methods become unused by the engine — remove
  their engine call sites; agent-facing record IO goes through a small
  `registry_client.rs` (reqwest against the server tier, token header, never logging the
  token).
- Modify: `bins/agent/src/lib.rs` — new job kinds Commit and Push (model.rs JobKind +=
  Commit, Push; api creates them from the new user routes in Task 16); auto-commit timer
  task (default 300s, WSSNAP_AUTOCOMMIT_SECS) committing every locally-present live
  subvolume; agent config gains the server-tier base URL WS_REGISTRY_URL.
- Test: engine_ops tests split accordingly (commit leaves nothing remote; push uploads
  exactly the unpushed set; commit-commit-push yields both layers; pull of a
  never-pushed commit fails clean). Agent loop test covers a Commit then Push job round
  trip against an in-process server-tier app.

Commit: `Split commit from push and rewire the agent to the registry`.

### Task 16: Frontend api slims down + volume browse

**Files:**
- Modify: `crates/workspaces/src/api.rs` — add POST /v1/workspaces/{id}/commit|push,
  /v1/environments/{id}/commit|push (job creation), GET /v1/volumes[,/{name}/history,
  /{name}/refs] (served by reading the volume DB through the server tier's routed reads —
  the api already talks to the server tier over the peer listener for browse; follow that
  exact pattern from crates/api), workspace docs carry `volume` pointer (model.rs rename
  ref_->volume with serde alias for old docs).
- Test: api_user.rs — commit/push create jobs; volume history browse round-trips against
  an in-process server app.

Commit: `Add commit, push and volume browse to the frontend api`.

### Task 17: Test migration, e2e, docs

**Files:**
- Modify: tests/ws_e2e.sh — the flow becomes: create ws → write → COMMIT → verify local
  history + nothing remote → PUSH → verify volume history via GET /v1/volumes → fork (from
  pushed commit) → clone → env up/down (down = commit+push once) → registry history shows
  both volumes. CLAUDE.md + README updated for the registry namespace and verb split.
- Full VM + live e2e verification is controller-side.

Commit: `Migrate the e2e and docs to the registry model`.
