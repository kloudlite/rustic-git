//! The agent loop: register with the control-plane API, then long-poll for jobs and run them
//! against the local `Engine`. Split out of `main.rs` so `tests/loop.rs` can spawn the same
//! loop against an in-process API server.
//!
//! Each job runs via `spawn_blocking` (its own OS thread, its own tiny current-thread runtime),
//! not `tokio::spawn` on the shared reactor: `Engine::push`/`squash` block on `ws_lock`'s
//! synchronous `libc::flock`, and a `LocalSet`/single-reactor-thread design would let one
//! workspace's lock wait freeze every other in-flight job. `spawn_blocking` also sidesteps
//! `WsClone`'s `&dyn Fn` stop/start hooks (no `+Send` bound in `engine::ops.rs`, out of scope
//! to change here) — they never have to cross an actual cross-thread `.await` boundary.

use rustic_git_workspaces::engine::{blob, Engine, Pool};
use rustic_git_workspaces::model::{Job, JobKind, LayerKind, LineageEntry};
use rustic_git_workspaces::store::MetaStore;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub mod container;

/// Env-derived config shared by `run` and the `squash` subcommand — both need the same Engine.
pub struct Config {
    pub api_url: String,
    pub region: String,
    pub agent_token: String,
    pub pool: String,
    pub hostname: String,
    pub cpu: u32,
    pub mem_mb: u64,
    pub disk_gb: u64,
}

impl Config {
    /// Base URL for the agent work surface. `WS_REGISTRY_URL` names the server tier now that
    /// Task 14 moved register/work/done/failed off `bins/api` — `WS_API_URL` is the old name,
    /// kept as a fallback (with a deprecation notice) only because the e2e deploy still exports
    /// it; drop the fallback once Task 17 repoints that.
    pub fn from_env() -> Config {
        let api_url = match std::env::var("WS_REGISTRY_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => match std::env::var("WS_API_URL") {
                Ok(v) if !v.is_empty() => {
                    eprintln!(
                        "rustic-git-agent: WS_API_URL is deprecated for the agent work surface, use WS_REGISTRY_URL (points at the server tier, not bins/api)"
                    ); // ponytail: eprintln
                    v
                }
                _ => "http://127.0.0.1:8081".into(),
            },
        };
        Config {
            api_url,
            region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
            agent_token: std::env::var("WS_AGENT_TOKEN").unwrap_or_default(),
            pool: std::env::var("WS_POOL").unwrap_or_else(|_| "/mnt/wspool".into()),
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "agent".into()),
            cpu: std::env::var("WS_CPU").ok().and_then(|v| v.parse().ok()).unwrap_or(4),
            mem_mb: std::env::var("WS_MEM_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(16384),
            disk_gb: std::env::var("WS_DISK_GB").ok().and_then(|v| v.parse().ok()).unwrap_or(128),
        }
    }
}

/// Build the region's blob store: Azure when `AZURE_ACCOUNT` is set, else the `S3_URL` MinIO
/// fallback used by tests (`engine::blob` already has both constructors).
pub fn blob_store() -> Arc<dyn object_store::ObjectStore> {
    match (std::env::var("AZURE_ACCOUNT"), std::env::var("AZURE_KEY"), std::env::var("AZURE_CONTAINER")) {
        (Ok(a), Ok(k), Ok(c)) => blob::region_store(&a, &k, &c),
        _ => blob::s3_store(),
    }
}

/// Construct the `Engine` this agent (or the detached `squash` subcommand) operates against.
/// `registry_url`/`agent_token` point the engine's `RegistryClient` at the same server tier
/// (and same token) the agent already uses for `register`/`work`/`jobs/*` — `WS_REGISTRY_URL`
/// serves both surfaces.
pub fn build_engine(pool: &str, meta: Arc<dyn MetaStore>, registry_url: &str, agent_token: &str) -> Engine {
    Engine::new(
        Pool::new(pool),
        blob_store(),
        meta,
        rustic_git_workspaces::registry_client::RegistryClient::new(registry_url, agent_token),
    )
}

/// Same `COSMOS_ENDPOINT`/`COSMOS_KEY`/`COSMOS_DB` convention as `bins/api`: unset means dev,
/// an in-memory store (fine for the agent's own tests, since the API side and this side must
/// share one store — real deployments always set these to point at the same Cosmos DB the API
/// bin uses).
pub async fn meta_store_from_env() -> Result<Arc<dyn MetaStore>, String> {
    match std::env::var("COSMOS_ENDPOINT") {
        Ok(endpoint) if !endpoint.is_empty() => {
            let key = std::env::var("COSMOS_KEY").map_err(|_| "COSMOS_KEY required with COSMOS_ENDPOINT".to_string())?;
            let db = std::env::var("COSMOS_DB").unwrap_or_else(|_| "rustic-git".into());
            Ok(Arc::new(
                rustic_git_workspaces::cosmos::CosmosStore::new(&endpoint, &key, &db)
                    .await
                    .map_err(|e| format!("connecting to cosmos: {e:?}"))?,
            ))
        }
        _ => Ok(Arc::new(rustic_git_workspaces::store::MemStore::new())),
    }
}

/// Persisted at `{pool}/agent-id` so a restarted agent process reuses its identity instead of
/// re-registering (and orphaning its old `AgentDoc`) every boot.
fn agent_id_path(pool: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("agent-id")
}

async fn register(client: &reqwest::Client, cfg: &Config) -> Result<String, String> {
    let path = agent_id_path(&cfg.pool);
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let resp = client
        .post(format!("{}/vol-agent/register", cfg.api_url))
        .header(rustic_git_workspaces::api::WS_AGENT_HEADER, &cfg.agent_token)
        .json(&json!({
            "region": cfg.region,
            "hostname": cfg.hostname,
            "pool": cfg.pool,
            "capacity": {"cpu": cfg.cpu, "mem_mb": cfg.mem_mb, "disk_gb": cfg.disk_gb},
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("register: {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let id = body["id"].as_str().ok_or("register: no id in response")?.to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    Ok(id)
}

/// One in-flight job costs a flat 1 cpu / 1024mb — a real cgroup-accurate measurement is more
/// than a job that only shells `docker compose` and `btrfs send` needs.
/// ponytail: flat per-job estimate, not real cgroup accounting; upgrade if jobs get heavier.
const JOB_CPU: u32 = 1;
const JOB_MEM_MB: u64 = 1024;

/// Runs the register + long-poll + dispatch loop forever. `engine` is shared across jobs (its
/// per-workspace flock serializes conflicting work); at most 4 jobs run concurrently via the
/// semaphore.
pub async fn run(cfg: Config) -> Result<(), String> {
    let meta = meta_store_from_env().await?;
    let engine = Arc::new(build_engine(&cfg.pool, meta, &cfg.api_url, &cfg.agent_token));
    run_with_engine(cfg, engine).await
}

/// Same loop as `run`, but takes an already-built `Engine` — the seam `tests/loop.rs` uses to
/// share the in-process `MemStore` between the test's API server and the agent under test.
pub async fn run_with_engine(cfg: Config, engine: Arc<Engine>) -> Result<(), String> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(40)).build().map_err(|e| e.to_string())?;
    let agent_id = register(&client, &cfg).await?;
    eprintln!("rustic-git-agent {agent_id} registered in {}", cfg.region);

    spawn_janitor(engine.clone(), cfg.pool.clone());

    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let inflight = Arc::new(AtomicU32::new(0));

    loop {
        let used_cpu = inflight.load(Ordering::Relaxed) * JOB_CPU;
        let used_mem = inflight.load(Ordering::Relaxed) as u64 * JOB_MEM_MB;
        let url = format!(
            "{}/vol-agent/work?agent={agent_id}&used_cpu={used_cpu}&used_mem_mb={used_mem}&used_disk_gb=0",
            cfg.api_url
        );
        let resp = match client.get(&url).header(rustic_git_workspaces::api::WS_AGENT_HEADER, &cfg.agent_token).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agent: work poll: {e}"); // ponytail: eprintln
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            continue;
        }
        if !resp.status().is_success() {
            eprintln!("agent: work poll: {}", resp.status()); // ponytail: eprintln
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        let job: Job = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("agent: bad job body: {e}"); // ponytail: eprintln
                continue;
            }
        };

        let permit = sem.clone().acquire_owned().await.map_err(|e| e.to_string())?;
        let engine = engine.clone();
        let client = client.clone();
        let cfg_api = cfg.api_url.clone();
        let cfg_tok = cfg.agent_token.clone();
        let inflight = inflight.clone();
        inflight.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _permit = permit;
            let job_id = job.id.clone();
            let outcome = tokio::task::spawn_blocking(move || run_job_blocking(&engine, &job))
                .await
                .unwrap_or_else(|e| Err(format!("job panicked: {e}")));
            inflight.fetch_sub(1, Ordering::Relaxed);
            report(&client, &cfg_api, &cfg_tok, &job_id, outcome).await;
        });
    }
}

/// Local storage janitor: every `WSSNAP_JANITOR_SECS` (default 600), reclaims local disk that a
/// pushed history no longer needs. Retention
/// rule: PUSHED history is re-derivable from the registry at any time (blobs are immutable
/// there), so a pushed local snapshot is pure cache — reclaimed once it's neither the tip (the
/// parent `commit_core`'s `btrfs send -p` needs for the NEXT delta) nor the current block-layer
/// base (the snapshot name `Engine::squash_inner`'s graft-after-race logic still looks up by
/// name while a squash is in flight). Unpushed anything is the ONLY local copy of that data and
/// is never touched — this whole function skips any lineage entry still marked `unpushed`.
fn spawn_janitor(engine: Arc<Engine>, pool: String) {
    let secs: u64 = std::env::var("WSSNAP_JANITOR_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(secs));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let voldir = std::path::Path::new(&pool).join("vol");
            let Ok(entries) = std::fs::read_dir(&voldir) else { continue };
            let mut reclaimed = 0usize;
            // A blob referenced by ANY volume's still-unpushed lineage entry must survive the
            // global stage sweep below, even though the stage dir isn't scoped per volume.
            let mut unpushed_blobs = std::collections::HashSet::new();
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
                let lineage = engine.pool.lineage(&id);
                unpushed_blobs.extend(lineage.iter().filter(|e| e.unpushed).map(|e| e.blob.clone()));
                reclaimed += janitor_volume_snapshots(&engine, &id, &lineage);
            }
            let staged = janitor_sweep_stage(&engine, &unpushed_blobs);
            if reclaimed > 0 || staged > 0 {
                eprintln!("agent: janitor reclaimed {reclaimed} snapshot(s), {staged} stray stage file(s)"); // ponytail: eprintln
            }
        }
    });
}

/// Snapshot-reclaim pass for one volume's lineage, split out of `spawn_janitor`'s loop so it can
/// be exercised directly by a test without waiting on the interval. Never touches staged files
/// (that's `janitor_sweep_stage`'s job, done once globally per tick, not per volume).
fn janitor_volume_snapshots(engine: &Engine, id: &str, lineage: &[LineageEntry]) -> usize {
    let Some(tip) = lineage.last() else { return 0 };
    let tip_name = tip.snap_name().to_string();
    let block_base = lineage.iter().rev().find(|e| e.kind == LayerKind::Block).map(|e| e.snap_name().to_string());
    // A local-first clone (`Engine::clone_local_snapshot`) copies the source's lineage VERBATIM,
    // so a snapshot that's a non-tip, already-pushed entry for THIS volume can still be another
    // volume's tip or `btrfs send -p` parent — reclaiming it here would break that sibling's next
    // push. Same cross-volume rule `cleanup_local` applies before a delete.
    let elsewhere = other_lineage_snap_names(engine, id);
    let root = engine.pool.snap_root(id);
    let mut reclaimed = 0;
    for e in lineage {
        if e.unpushed {
            continue;
        }
        let name = e.snap_name();
        if name == tip_name || Some(name) == block_base.as_deref() || elsewhere.contains(name) {
            continue;
        }
        let snap = root.join(name);
        if snap.exists() {
            btrfs_delete(&snap, id);
            reclaimed += 1;
        }
    }
    reclaimed
}

/// Removes any staged layer/meta file (`{blob}.zst`/`{blob}.json` under `Pool::stage_dir`) whose
/// blob id isn't in `keep` — orphaned by a crash between staging and push clearing it, since a
/// clean push already deletes its own. Global (not per-volume): the stage dir is shared pool
/// state, so `keep` must already be the union across every volume's unpushed entries.
fn janitor_sweep_stage(engine: &Engine, keep: &std::collections::HashSet<String>) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.stage_dir()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        if !keep.contains(&stem) && std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// Runs one job to completion on its own OS thread (see the module doc for why): builds a tiny
/// current-thread runtime just for this job's async `Engine` calls, so the flock wait inside
/// `push`/`squash` and `WsClone`'s non-`Send` closures never touch the shared reactor.
fn run_job_blocking(engine: &Engine, job: &Job) -> Result<serde_json::Value, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(run_job(engine, job))
}

async fn report(client: &reqwest::Client, api: &str, token: &str, job_id: &str, outcome: Result<serde_json::Value, String>) {
    let (path, body) = match outcome {
        Ok(result) => (format!("{api}/vol-agent/jobs/{job_id}/done"), json!({"result": result})),
        Err(error) => {
            // The error also lands in the job doc, but nothing user-facing reads job docs yet
            // (known v1 gap) — the agent log is the only place an operator can see WHY.
            eprintln!("agent: job {job_id} failed: {error}"); // ponytail: eprintln
            (format!("{api}/vol-agent/jobs/{job_id}/failed"), json!({"error": error}))
        }
    };
    if let Err(e) = client.post(&path).header(rustic_git_workspaces::api::WS_AGENT_HEADER, token).json(&body).send().await {
        eprintln!("agent: reporting job {job_id}: {e}"); // ponytail: eprintln
    }
}

fn ws_from_payload(payload: &serde_json::Value, key: &str) -> Result<String, String> {
    payload[key].as_str().map(|s| s.to_string()).ok_or_else(|| format!("payload missing {key}"))
}

/// Fetches the `Workspace` doc named by `payload[key]`, owned by `payload["owner"]`.
async fn ws_doc(
    engine: &Engine,
    payload: &serde_json::Value,
    key: &str,
) -> Result<rustic_git_workspaces::model::Workspace, String> {
    let id = ws_from_payload(payload, key)?;
    let owner = payload["owner"].as_str().ok_or("payload missing owner")?;
    let (w, _) = engine.meta.get_ws(owner, &id).await.map_err(|e| format!("{e:?}"))?.ok_or("workspace not found")?;
    Ok(w)
}

/// `Engine::push`'s detached `squash <ws-id>` child (`ops.rs`) is spawned with only the
/// workspace id, no owner — so the id -> owner mapping has to be recoverable locally.
/// `MetaStore` has no owner-less lookup (Cosmos partitions by owner) and adding one is out of
/// this task's scope, so the agent leaves a breadcrumb on the pool itself, right where the
/// lineage file already lives.
pub fn owner_file(pool: &str, ws_id: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("vol").join(format!("{ws_id}.owner"))
}

fn record_owner(pool: &str, ws: &rustic_git_workspaces::model::Workspace) {
    let _ = std::fs::create_dir_all(std::path::Path::new(pool).join("vol"));
    let _ = std::fs::write(owner_file(pool, &ws.id), &ws.owner);
}

async fn run_job(engine: &Engine, job: &Job) -> Result<serde_json::Value, String> {
    match job.kind {
        JobKind::WsCreate => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            engine.init(&w).await.map_err(|e| e.to_string())?;
            container::start(&w.id, &w.image, &engine.pool.live(&w.id)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsPush => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            // Defensive backfill: a workspace pushed without a create/clone/restore on THIS pool
            // (e.g. a re-registered agent, a moved pool) still needs the owner breadcrumb for
            // `push`'s auto-squash to find later.
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            let out = engine.push(&w, None).await.map_err(|e| e.to_string())?;
            Ok(json!({"layer": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        // Shared between workspaces and environments — the api (`crates/workspaces/src/api.rs`'s
        // `push_ws`/`push_env`) sets `workspace` or `environment` in the payload depending on
        // which route was hit, so branch on which key is present rather than trusting the job
        // kind alone to say which engine call applies.
        JobKind::Push if job.payload.get("environment").is_some() => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let message = job.payload.get("message").and_then(|v| v.as_str());
            // Same "one subvolume, null live_state" shape as `EnvDown`'s own push.
            let out = engine.push_env(&owner, &id, &serde_json::Value::Null, message).await.map_err(|e| e.to_string())?;
            Ok(json!({"commit": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        JobKind::Push => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            let message = job.payload.get("message").and_then(|v| v.as_str());
            let out = engine.push(&w, message).await.map_err(|e| e.to_string())?;
            Ok(json!({"commit": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        JobKind::WsRestore => {
            let dst = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &dst);
            let snap_id = job.payload.get("snapshot_id").and_then(|v| v.as_str()).ok_or("payload missing snapshot_id")?;
            let src_id = ws_from_payload(&job.payload, "src_workspace")?;
            // `src_owner` isn't in every caller's payload yet; same-owner restore is the common
            // case, so fall back to the job's own `owner`.
            let owner_fallback = job.payload["owner"].as_str().ok_or("payload missing owner")?;
            let src_owner = job.payload.get("src_owner").and_then(|v| v.as_str()).unwrap_or(owner_fallback);
            engine.restore(src_owner, &src_id, snap_id, &dst).await.map_err(|e| e.to_string())?;
            container::start(&dst.id, &dst.image, &engine.pool.live(&dst.id)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        // Shared with workspace clone, same "branch on which payload key is present" idiom
        // `Push` already uses — `clone_env` (`crates/workspaces/src/api.rs`) sets `environment`/
        // `src`/`stop_project` instead of `workspace`/`src`/`stop_container`.
        JobKind::WsClone if job.payload.get("environment").is_some() => {
            let (owner, dst_id) = env_owner_id(&job.payload)?;
            let src_id = job.payload["src"].as_str().ok_or("payload missing src")?.to_string();
            let (dst, _) = engine.meta.get_env(&owner, &dst_id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            let (src, _) = engine.meta.get_env(&owner, &src_id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            let stop_project = job.payload.get("stop_project").and_then(|v| v.as_str()).map(String::from);
            // `EnvUp`/`EnvDown`'s done handlers keep this current — cheaper and just as reliable
            // as a docker inspect, and there's no single "the env's container" to inspect anyway
            // (a compose project can be many).
            let running = src.state == rustic_git_workspaces::model::EnvState::Running;
            if running {
                let stop = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                    if let Some(p) = &stop_project {
                        compose(p, "stop")?;
                    }
                    Ok(())
                };
                let start = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                    if let Some(p) = &stop_project {
                        compose(p, "start")?;
                    }
                    Ok(())
                };
                engine.clone_running_local(&src.id, &dst.id, &stop, &start).await.map_err(|e| e.to_string())?;
            } else {
                engine.clone_local_ids(&src.owner, &src.id, &dst.id).await.map_err(|e| e.to_string())?;
            }
            // Bring the clone up exactly like `EnvUp` does — the volume clone only copied files;
            // nothing has rendered a compose project or started containers for `dst` yet.
            let live = engine.pool.live(&dst.id);
            mkdir_env_mounts(&live, &dst).map_err(|e| e.to_string())?;
            rustic_git_workspaces::engine::compose::up(&dst, &live, &env_dir(&engine.pool, &dst.id)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsClone => {
            let dst = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &dst);
            let src = ws_doc(engine, &job.payload, "src").await?;
            let projects: Vec<String> = job
                .payload
                .get("stop_projects")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            // `stop_projects` (env-era compose projects) is kept for the future env-clone this
            // was always meant to grow into; the source workspace's own container is the thing
            // actually running today, so it gets the same pause-around-the-clone treatment.
            let stop_container = job.payload.get("stop_container").and_then(|v| v.as_str()).map(String::from);
            // Only a RUNNING source needs the stop/prefetch/start dance of `clone_running` — a
            // stopped (or never-started) source goes through `Engine::clone_local`, which is
            // local-first and skips the network entirely when possible.
            let running = stop_container.as_deref().map(container::is_running).transpose().map_err(|e| e.to_string())?.unwrap_or(false);
            if running {
                let stop = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                    for p in &projects {
                        compose(p, "stop")?;
                    }
                    if let Some(c) = &stop_container {
                        docker_stop_name(c)?;
                    }
                    Ok(())
                };
                let start = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                    for p in &projects {
                        compose(p, "start")?;
                    }
                    if let Some(c) = &stop_container {
                        docker_start_name(c)?;
                    }
                    Ok(())
                };
                engine.clone_running(&src, &dst, &stop, &start).await.map_err(|e| e.to_string())?;
            } else {
                engine.clone_local(&src, &dst).await.map_err(|e| e.to_string())?;
            }
            container::start(&dst.id, &dst.image, &engine.pool.live(&dst.id)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsStart => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            container::start(&w.id, &w.image, &engine.pool.live(&w.id)).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsStop => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            container::stop(&w.id).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsDelete => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            container::remove(&w.id).map_err(|e| e.to_string())?;
            cleanup_local(engine, &w.id);
            Ok(json!({}))
        }
        JobKind::EnvUp => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let (env, _) = engine.meta.get_env(&owner, &id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            let live = engine.pool.live(&env.id);
            if !live.exists() {
                match &env.volume {
                    Some(r) => {
                        engine.pull_env(&env.id, r).await.map_err(|e| e.to_string())?;
                    }
                    None => engine.create_subvol(&env.id).map_err(|e| e.to_string())?,
                }
            }
            mkdir_env_mounts(&live, &env).map_err(|e| e.to_string())?;
            rustic_git_workspaces::engine::compose::up(&env, &live, &env_dir(&engine.pool, &env.id))
                .map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::EnvDown => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let (env, _) = engine.meta.get_env(&owner, &id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            rustic_git_workspaces::engine::compose::down(&env, &env_dir(&engine.pool, &env.id)).map_err(|e| e.to_string())?;
            // Spec open question 3: always push on down — safest default, revisit on cost. One
            // push of the env's own subvolume covers every mounted volume atomically (the
            // decision this whole task exists to enforce), unlike the old per-mounted-workspace
            // loop this replaces.
            engine.push_env(&owner, &env.id, &serde_json::Value::Null, None).await.map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::EnvDelete => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let (env, _) = engine.meta.get_env(&owner, &id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            rustic_git_workspaces::engine::compose::down(&env, &env_dir(&engine.pool, &env.id)).map_err(|e| e.to_string())?;
            // Same "durable last state before the subvolume disappears" rule as EnvDown — an
            // env that's deleted without ever pushing its final state would lose it for good.
            engine.push_env(&owner, &env.id, &serde_json::Value::Null, None).await.map_err(|e| e.to_string())?;
            cleanup_local(engine, &env.id);
            Ok(json!({}))
        }
    }
}

fn env_owner_id(payload: &serde_json::Value) -> Result<(String, String), String> {
    let owner = payload["owner"].as_str().ok_or("payload missing owner")?.to_string();
    let id = payload["environment"].as_str().ok_or("payload missing environment")?.to_string();
    Ok((owner, id))
}

/// Where an environment's rendered `docker-compose.yml` lives on this pool.
fn env_dir(pool: &rustic_git_workspaces::engine::Pool, env_id: &str) -> std::path::PathBuf {
    pool.root.join("env").join(env_id)
}

/// Every declared volume is a folder inside the env's ONE subvolume — mkdir -p each before
/// `compose::up` so the bind source always exists. Shared by `EnvUp` and an env clone's
/// bring-up (same requirement, same env doc shape either way).
fn mkdir_env_mounts(live: &std::path::Path, env: &rustic_git_workspaces::model::Environment) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for svc in &env.services {
        for m in &svc.mounts {
            if seen.insert(m.folder.clone()) {
                std::fs::create_dir_all(live.join("volumes").join(&m.folder)).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Full local reclaim for a deleted workspace/environment: the live subvolume, every RO snapshot
/// its local lineage names, staged (still-unpushed) layer/meta files, the pool's own
/// `.lineage`/`.owner`/`.lock`/`.squash-err` bookkeeping, and finally the `{pool}/vol/{id}`
/// directory itself. Registry/blob bytes are NEVER touched here — blobs are immutable and shared
/// across siblings (a clone's history references the same blob ids), deleted only by an explicit
/// blob-delete path or GC, never by a workspace/environment delete. Best-effort throughout
/// (eprintln, never fails the job): a retried delete job must still finish even if a prior
/// attempt got partway through.
/// Union of every OTHER volume's unpushed lineage blob ids on this pool (excludes `exclude_id`
/// itself) — used by `cleanup_local` to keep a stage file a local-first clone still shares.
fn other_unpushed_blobs(engine: &Engine, exclude_id: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(engine.pool.root.join("vol")) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if id == exclude_id {
            continue;
        }
        out.extend(engine.pool.lineage(&id).into_iter().filter(|e| e.unpushed).map(|e| e.blob));
    }
    out
}

/// Every OTHER volume's lineage snap names on this pool (excludes `exclude_id` itself, every
/// entry not just unpushed ones) — a local-first clone (`Engine::clone_local_snapshot`) copies
/// the source's lineage VERBATIM, so `recv/{snap}` can be the source's tip/parent AND a clone's
/// own tip/parent at once; both `cleanup_local` (deleting the source must not strip a snapshot
/// a clone still needs) and the janitor's snapshot sweep (reclaiming one volume's non-tip
/// history must not strip another volume's tip/parent) key off this before deleting anything.
/// ponytail: one `vol/` scan per caller, same O(n) cost class as `other_unpushed_blobs`; fine at
/// expected per-pool volume counts.
fn other_lineage_snap_names(engine: &Engine, exclude_id: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(engine.pool.root.join("vol")) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if id == exclude_id {
            continue;
        }
        out.extend(engine.pool.lineage(&id).iter().map(|e| e.snap_name().to_string()));
    }
    out
}

fn cleanup_local(engine: &Engine, id: &str) {
    let lineage = engine.pool.lineage(id);
    let root = engine.pool.snap_root(id);
    let live = engine.pool.live(id);
    if live.exists() {
        btrfs_delete(&live, id);
    }
    // A local-first clone (`Engine::clone_local`) shares its inherited unpushed entries' staged
    // files with the source by blob id (`Pool::stage_dir` is pool-global) rather than copying
    // them — deleting the source must not strip a stage file a sibling clone still needs to push.
    // Same scan `spawn_janitor`'s stage sweep uses, just excluding this volume (being deleted)
    // from the "still referenced" set.
    let elsewhere = other_unpushed_blobs(engine, id);
    // Same sharing, one level up: `clone_local_snapshot` copies the source's lineage VERBATIM,
    // so `recv/{snap}` can be BOTH this volume's own history AND a clone's tip/parent at once —
    // deleting it here would leave the clone's next push sending `-p` against a snapshot that no
    // longer exists (the real bug this scan closes).
    let elsewhere_snaps = other_lineage_snap_names(engine, id);
    for e in &lineage {
        let snap = root.join(e.snap_name());
        if snap.exists() && !elsewhere_snaps.contains(e.snap_name()) {
            btrfs_delete(&snap, id);
        }
        if e.unpushed && !elsewhere.contains(&e.blob) {
            let _ = std::fs::remove_file(engine.pool.stage_path(&e.blob));
            let _ = std::fs::remove_file(engine.pool.stage_meta_path(&e.blob));
        }
    }
    let vol_root = engine.pool.root.join("vol");
    for ext in ["lineage", "owner", "lock", "squash-err"] {
        let _ = std::fs::remove_file(vol_root.join(format!("{id}.{ext}")));
    }
    let voldir = engine.pool.voldir(id);
    // A block-restored workspace's voldir is itself a loop mount (see `Pool::snap_root`'s doc) —
    // unmount before rmdir, else the directory is busy and never goes away.
    if rustic_git_workspaces::engine::is_mountpoint(&voldir) {
        let _ = std::process::Command::new("umount").arg(&voldir).output();
    }
    if let Err(e) = std::fs::remove_dir_all(&voldir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("agent: cleanup {id}: remove {}: {e}", voldir.display()); // ponytail: eprintln
        }
    }
}

fn btrfs_delete(path: &std::path::Path, id: &str) {
    match std::process::Command::new("btrfs").args(["subvolume", "delete", path.to_str().unwrap()]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => eprintln!(
            "agent: cleanup {id}: btrfs subvolume delete {}: {}", // ponytail: eprintln
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => eprintln!("agent: cleanup {id}: btrfs subvolume delete {}: {e}", path.display()), // ponytail: eprintln
    }
}

/// `stop`/`start` by exact container name — distinct from `container::stop`, which derives the
/// name from a workspace id; `WsClone`'s hooks are handed the SOURCE's already-formatted
/// `ws-{src-id}` name straight from the job payload.
/// Absent container == already stopped, same tolerance as `container::stop`/`container::remove`
/// — the source of a clone can race a delete of that same source between scheduling and running.
fn docker_stop_name(cname: &str) -> Result<(), rustic_git_workspaces::engine::EngErr> {
    let out = std::process::Command::new("docker")
        .args(["stop", cname])
        .output()
        .map_err(|e| rustic_git_workspaces::engine::EngErr(format!("spawn docker stop: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("No such container") {
        return Ok(());
    }
    Err(rustic_git_workspaces::engine::EngErr(format!("docker stop {cname}: {stderr}")))
}

fn docker_start_name(cname: &str) -> Result<(), rustic_git_workspaces::engine::EngErr> {
    let out = std::process::Command::new("docker")
        .args(["start", cname])
        .output()
        .map_err(|e| rustic_git_workspaces::engine::EngErr(format!("spawn docker start: {e}")))?;
    if !out.status.success() {
        return Err(rustic_git_workspaces::engine::EngErr(format!(
            "docker start {cname}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn compose(project: &str, action: &str) -> Result<(), rustic_git_workspaces::engine::EngErr> {
    let out = std::process::Command::new("docker")
        .args(["compose", "-p", project, action])
        .output()
        .map_err(|e| rustic_git_workspaces::engine::EngErr(format!("spawn docker compose: {e}")))?;
    if !out.status.success() {
        return Err(rustic_git_workspaces::engine::EngErr(format!(
            "docker compose -p {project} {action}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod janitor_tests {
    use super::*;
    use rustic_git_workspaces::engine::have_btrfs;
    use rustic_git_workspaces::store::MemStore;

    /// Mirrors `crates/workspaces/tests/engine_pool.rs`'s `LoopbackPool`: a truncated sparse
    /// btrfs image, mounted for the test and unmounted on drop.
    struct LoopbackPool {
        pool: Pool,
        mount: std::path::PathBuf,
        _tmp: tempfile::TempDir,
    }
    impl LoopbackPool {
        fn new() -> LoopbackPool {
            let tmp = tempfile::tempdir().unwrap();
            let img = tmp.path().join("pool.img");
            let mount = tmp.path().join("mnt");
            std::fs::create_dir_all(&mount).unwrap();
            run(&["truncate", "-s", "1G", img.to_str().unwrap()]);
            run(&["mkfs.btrfs", "-q", img.to_str().unwrap()]);
            run(&["mount", "-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()]);
            let pool = Pool::new(mount.clone());
            std::fs::create_dir_all(pool.recv()).unwrap();
            std::fs::create_dir_all(pool.root.join("vol")).unwrap();
            LoopbackPool { pool, mount, _tmp: tmp }
        }
    }
    impl Drop for LoopbackPool {
        fn drop(&mut self) {
            let _ = std::process::Command::new("umount").arg(&self.mount).status();
        }
    }
    fn run(argv: &[&str]) {
        let st = std::process::Command::new(argv[0]).args(&argv[1..]).status().unwrap();
        assert!(st.success(), "{argv:?} failed");
    }

    fn stream_entry(blob: &str, unpushed: bool) -> LineageEntry {
        LineageEntry { kind: LayerKind::Stream, blob: blob.into(), snap: None, sha256: "sha".into(), unpushed }
    }

    #[test]
    fn keeps_only_tip_and_unpushed_reclaims_the_rest() {
        if !have_btrfs() {
            eprintln!("skipping: btrfs unavailable or not root");
            return;
        }
        let lp = LoopbackPool::new();
        for s in ["s1", "s2", "s3", "s4"] {
            run(&["btrfs", "subvolume", "create", lp.pool.recv().join(s).to_str().unwrap()]);
        }
        let id = "vol-janitor-1";
        // 3 pushed commits, then a 4th still-unpushed one (the current tip).
        let lineage = vec![stream_entry("s1", false), stream_entry("s2", false), stream_entry("s3", false), stream_entry("s4", true)];
        lp.pool.set_lineage(id, &lineage);
        std::fs::create_dir_all(lp.pool.stage_dir()).unwrap();
        std::fs::write(lp.pool.stage_meta_path("s4"), b"{}").unwrap();

        let engine = Engine::new(
            Pool::new(lp.pool.root.clone()),
            std::sync::Arc::new(object_store::memory::InMemory::new()),
            std::sync::Arc::new(MemStore::new()),
            rustic_git_workspaces::registry_client::RegistryClient::new("http://127.0.0.1:1", "unused"),
        );
        let reclaimed = janitor_volume_snapshots(&engine, id, &lineage);
        assert_eq!(reclaimed, 3, "the 3 pushed non-tip snapshots must be reclaimed");

        assert!(!lp.pool.recv().join("s1").exists());
        assert!(!lp.pool.recv().join("s2").exists());
        assert!(!lp.pool.recv().join("s3").exists());
        assert!(lp.pool.recv().join("s4").exists(), "the unpushed tip must never be touched");
        assert!(lp.pool.stage_meta_path("s4").exists(), "unpushed stage files must be left intact");
    }
}
