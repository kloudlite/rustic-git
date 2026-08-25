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
use rustic_git_workspaces::model::{Job, JobKind};
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

    spawn_autocommit(engine.clone(), cfg.pool.clone());

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

/// Every `WSSNAP_AUTOCOMMIT_SECS` (default 300), commits every workspace whose `live` subvolume
/// is present on this pool — a cheap, offline safety net so a workspace that's been running a
/// long time without an explicit push still has recent local history to push from. Owner comes
/// from `owner_file`'s breadcrumb (same one `push`'s detached squash relies on); a workspace
/// missing that breadcrumb, or whose `Workspace` doc lookup fails, is skipped rather than
/// failing the whole sweep — one bad entry must not starve the rest.
fn spawn_autocommit(engine: Arc<Engine>, pool: String) {
    let secs: u64 = std::env::var("WSSNAP_AUTOCOMMIT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(secs));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let wsdir = std::path::Path::new(&pool).join("ws");
            let Ok(entries) = std::fs::read_dir(&wsdir) else { continue };
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() || !p.join("live").exists() {
                    continue;
                }
                let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
                let Ok(owner) = std::fs::read_to_string(owner_file(&pool, &id)) else { continue };
                let owner = owner.trim();
                match engine.meta.get_ws(owner, &id).await {
                    Ok(Some((w, _))) => {
                        if let Err(e) = engine.commit_auto(&w).await {
                            eprintln!("agent: autocommit {id}: {e}"); // ponytail: eprintln
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("agent: autocommit {id}: workspace lookup: {e:?}"), // ponytail: eprintln
                }
            }
        }
    });
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
    std::path::Path::new(pool).join("ws").join(format!("{ws_id}.owner"))
}

fn record_owner(pool: &str, ws: &rustic_git_workspaces::model::Workspace) {
    let _ = std::fs::create_dir_all(std::path::Path::new(pool).join("ws"));
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
            // Defensive backfill: a workspace pushed without a create/fork/clone on THIS pool
            // (e.g. a re-registered agent, a moved pool) still needs the owner breadcrumb for
            // `push`'s auto-squash to find later.
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            // Kept as commit-then-push (not just push) so callers still creating `WsPush` jobs
            // (Task 16 wires the API to `Commit`/`Push` instead) see the same one-job behavior
            // as before the split.
            engine.commit(&w, None).await.map_err(|e| e.to_string())?;
            let out = engine.push(&w).await.map_err(|e| e.to_string())?;
            Ok(json!({"layer": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        // Both kinds are shared between workspaces and environments — the api
        // (`crates/workspaces/src/api.rs`'s `commit_ws`/`commit_env`) sets `workspace` or
        // `environment` in the payload depending on which route was hit, so branch on which key
        // is present rather than trusting the job kind alone to say which engine call applies.
        JobKind::Commit if job.payload.get("environment").is_some() => {
            let (_owner, id) = env_owner_id(&job.payload)?;
            let message = job.payload.get("message").and_then(|v| v.as_str());
            // Same "one subvolume, one commit, null live_state" shape as `EnvDown`'s auto-push.
            let layer = engine.commit_env(&id, &serde_json::Value::Null, message).await.map_err(|e| e.to_string())?;
            Ok(json!({"layer": layer}))
        }
        JobKind::Commit => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            let message = job.payload.get("message").and_then(|v| v.as_str());
            let layer = engine.commit(&w, message).await.map_err(|e| e.to_string())?;
            Ok(json!({"layer": layer}))
        }
        JobKind::Push if job.payload.get("environment").is_some() => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let out = engine.push_env(&owner, &id).await.map_err(|e| e.to_string())?;
            Ok(json!({"commit": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        JobKind::Push => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &w);
            let out = engine.push(&w).await.map_err(|e| e.to_string())?;
            Ok(json!({"commit": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        JobKind::WsFork => {
            let dst = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &dst);
            if let Some(snap_id) = job.payload.get("snapshot_id").and_then(|v| v.as_str()) {
                let src_id = ws_from_payload(&job.payload, "src_workspace")?;
                // `src_owner` isn't in every caller's payload yet (Task 16 wires the API route
                // that always sets it); same-owner fork/from-snapshot is the common case, so
                // fall back to the job's own `owner`.
                let owner_fallback = job.payload["owner"].as_str().ok_or("payload missing owner")?;
                let src_owner = job.payload.get("src_owner").and_then(|v| v.as_str()).unwrap_or(owner_fallback);
                engine.create_from_snapshot(src_owner, &src_id, snap_id, &dst).await.map_err(|e| e.to_string())?;
            } else {
                let src = ws_doc(engine, &job.payload, "src_workspace").await?;
                engine.fork(&src, &dst).await.map_err(|e| e.to_string())?;
            }
            container::start(&dst.id, &dst.image, &engine.pool.live(&dst.id)).map_err(|e| e.to_string())?;
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
            let live = engine.pool.live(&w.id);
            if live.exists() {
                std::process::Command::new("btrfs")
                    .args(["subvolume", "delete", live.to_str().unwrap()])
                    .output()
                    .map_err(|e| e.to_string())?;
            }
            // Snapshots stay in the object store — blobs are immutable, deleted only by an
            // explicit blob-delete path or GC, never by a workspace delete.
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
            // Every declared volume is a folder inside the env's ONE subvolume — mkdir -p each
            // before compose up so the bind source always exists.
            let mut seen = std::collections::HashSet::new();
            for svc in &env.services {
                for m in &svc.mounts {
                    if seen.insert(m.volume.clone()) {
                        std::fs::create_dir_all(live.join("volumes").join(&m.volume)).map_err(|e| e.to_string())?;
                    }
                }
            }
            rustic_git_workspaces::engine::compose::up(&env, &live, &env_dir(&engine.pool, &env.id))
                .map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::EnvDown => {
            let (owner, id) = env_owner_id(&job.payload)?;
            let (env, _) = engine.meta.get_env(&owner, &id).await.map_err(|e| format!("{e:?}"))?.ok_or("environment not found")?;
            rustic_git_workspaces::engine::compose::down(&env, &env_dir(&engine.pool, &env.id)).map_err(|e| e.to_string())?;
            // Spec open question 3: always push on down — safest default, revisit on cost. One
            // commit+push of the env's own subvolume covers every mounted volume atomically
            // (the decision this whole task exists to enforce), unlike the old per-mounted-
            // workspace loop this replaces. Split into commit then push (not one combined call)
            // for the same reason every other volume does: only push touches the network.
            engine.commit_env(&env.id, &serde_json::Value::Null, None).await.map_err(|e| e.to_string())?;
            engine.push_env(&owner, &env.id).await.map_err(|e| e.to_string())?;
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

/// `stop`/`start` by exact container name — distinct from `container::stop`, which derives the
/// name from a workspace id; `WsClone`'s hooks are handed the SOURCE's already-formatted
/// `ws-{src-id}` name straight from the job payload.
fn docker_stop_name(cname: &str) -> Result<(), rustic_git_workspaces::engine::EngErr> {
    let out = std::process::Command::new("docker")
        .args(["stop", cname])
        .output()
        .map_err(|e| rustic_git_workspaces::engine::EngErr(format!("spawn docker stop: {e}")))?;
    if !out.status.success() {
        return Err(rustic_git_workspaces::engine::EngErr(format!(
            "docker stop {cname}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
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
