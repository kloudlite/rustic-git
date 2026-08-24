//! The agent loop: register with the control-plane API, then long-poll for jobs and run them
//! against the local `Engine`. Split out of `main.rs` so `tests/loop.rs` can spawn the same
//! loop against an in-process API server.

use rustic_git_workspaces::engine::{blob, Engine, Pool};
use rustic_git_workspaces::model::{Job, JobKind};
use rustic_git_workspaces::store::MetaStore;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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
    pub fn from_env() -> Config {
        Config {
            api_url: std::env::var("WS_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
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
pub fn build_engine(pool: &str, meta: Arc<dyn MetaStore>) -> Engine {
    Engine::new(Pool::new(pool), blob_store(), meta)
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
        .post(format!("{}/v1/agent/register", cfg.api_url))
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
    let engine = Arc::new(build_engine(&cfg.pool, meta));
    run_with_engine(cfg, engine).await
}

/// Same loop as `run`, but takes an already-built `Engine` — the seam `tests/loop.rs` uses to
/// share the in-process `MemStore` between the test's API server and the agent under test.
pub async fn run_with_engine(cfg: Config, engine: Arc<Engine>) -> Result<(), String> {
    // `WsClone`'s stop/start hooks are `&dyn Fn` (no `+Send` bound — `engine::ops.rs`'s
    // signature, out of this task's scope to change), so a job's future is not `Send` and
    // can't cross `tokio::spawn`'s thread-pool boundary. `LocalSet` gives the same bounded
    // concurrency (`spawn_local`) without that requirement — jobs here are I/O/subprocess
    // bound, not CPU bound, so single-thread scheduling costs nothing real.
    let local = tokio::task::LocalSet::new();
    local.run_until(run_loop(cfg, engine)).await
}

async fn run_loop(cfg: Config, engine: Arc<Engine>) -> Result<(), String> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(40)).build().map_err(|e| e.to_string())?;
    let agent_id = register(&client, &cfg).await?;
    eprintln!("rustic-git-agent {agent_id} registered in {}", cfg.region);

    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let inflight = Arc::new(AtomicU32::new(0));

    loop {
        let used_cpu = inflight.load(Ordering::Relaxed) * JOB_CPU;
        let used_mem = inflight.load(Ordering::Relaxed) as u64 * JOB_MEM_MB;
        let url = format!(
            "{}/v1/agent/work?agent={agent_id}&used_cpu={used_cpu}&used_mem_mb={used_mem}&used_disk_gb=0",
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
        tokio::task::spawn_local(async move {
            let _permit = permit;
            let outcome = run_job(&engine, &job).await;
            inflight.fetch_sub(1, Ordering::Relaxed);
            report(&client, &cfg_api, &cfg_tok, &job.id, outcome).await;
        });
    }
}

async fn report(client: &reqwest::Client, api: &str, token: &str, job_id: &str, outcome: Result<serde_json::Value, String>) {
    let (path, body) = match outcome {
        Ok(result) => (format!("{api}/v1/agent/jobs/{job_id}/done"), json!({"result": result})),
        Err(error) => (format!("{api}/v1/agent/jobs/{job_id}/failed"), json!({"error": error})),
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
            Ok(json!({}))
        }
        JobKind::WsPush => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
            let out = engine.push(&w).await.map_err(|e| e.to_string())?;
            Ok(json!({"layer": out.layer, "sha": out.sha, "layers": out.layers}))
        }
        JobKind::WsFork => {
            let dst = ws_doc(engine, &job.payload, "workspace").await?;
            record_owner(&engine.pool.root.to_string_lossy(), &dst);
            if let Some(snap_id) = job.payload.get("snapshot_id").and_then(|v| v.as_str()) {
                let src_id = ws_from_payload(&job.payload, "src_workspace")?;
                engine.create_from_snapshot(&src_id, snap_id, &dst).await.map_err(|e| e.to_string())?;
            } else {
                let src = ws_doc(engine, &job.payload, "src_workspace").await?;
                engine.fork(&src, &dst).await.map_err(|e| e.to_string())?;
            }
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
            let stop = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                for p in &projects {
                    compose(p, "stop")?;
                }
                Ok(())
            };
            let start = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
                for p in &projects {
                    compose(p, "start")?;
                }
                Ok(())
            };
            engine.clone_running(&src, &dst, &stop, &start).await.map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        JobKind::WsDelete => {
            let w = ws_doc(engine, &job.payload, "workspace").await?;
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
        JobKind::EnvUp | JobKind::EnvDown => Err("environments not implemented until Task 10".into()),
    }
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
