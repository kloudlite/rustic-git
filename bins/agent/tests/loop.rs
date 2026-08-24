//! End-to-end: an in-process `/v1` API server (MemStore) plus a real agent loop
//! (`rustic_git_agent::run_with_engine`) running against a loopback btrfs pool, driven through
//! `WsCreate` then `WsPush`. Gated on `have_btrfs()` — same reason as every other engine test
//! (this Mac, non-root CI).

use object_store::memory::InMemory;
use object_store::ObjectStore;
use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::engine::{have_btrfs, Engine, Pool};
use rustic_git_workspaces::model::{Region, WsState};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "agent-loop-test-tok";

/// Mirrors `crates/workspaces/tests/engine_ops.rs`'s `LoopbackPool`: a truncated sparse image,
/// mounted for the test and unmounted on drop.
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
        run(&["truncate", "-s", "4G", img.to_str().unwrap()]);
        run(&["mkfs.btrfs", "-q", img.to_str().unwrap()]);
        run(&["mount", "-o", "loop", img.to_str().unwrap(), mount.to_str().unwrap()]);
        let pool = Pool::new(mount.clone());
        std::fs::create_dir_all(pool.recv()).unwrap();
        std::fs::create_dir_all(pool.root.join("ws")).unwrap();
        std::fs::create_dir_all(pool.root.join("img")).unwrap();
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

#[tokio::test]
async fn create_then_push_reaches_ready_with_a_snapshot() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }

    // ── API server, in-process, short poll window ──
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt, HashSet::new());
    state.agent_poll_window = Duration::from_millis(500);
    state.agent_poll_interval = Duration::from_millis(30);
    let state = Arc::new(state);
    store
        .put_region(&Region {
            id: "centralindia".into(),
            name: "Central India".into(),
            storage_account: "acct".into(),
            blob_container: "wslayers".into(),
            status: "active".into(),
            agent_token: TOKEN.into(),
        })
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let base = format!("http://{addr}");

    // ── agent loop, against the SAME MemStore (real deployments point both at the same
    // Cosmos DB) so the engine's `get_ws`/`get_snapshot` calls resolve. ──
    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(Pool::new(lp.pool.root.clone()), blob_store, store.clone() as Arc<dyn MetaStore>));
    let cfg = rustic_git_agent::Config {
        api_url: base.clone(),
        region: "centralindia".into(),
        agent_token: TOKEN.into(),
        pool: lp.pool.root.to_string_lossy().to_string(),
        hostname: "test-agent".into(),
        cpu: 4,
        mem_mb: 16384,
        disk_gb: 128,
    };
    tokio::spawn(rustic_git_agent::run_with_engine(cfg, engine));

    let owner = "alice";
    let ws_id = {
        // create_ws requires the real MemStore's workspace record to carry `region` matching
        // the agent's region and `owner` — same route the browser hits.
        let w = rustic_git_workspaces::model::Workspace {
            id: "ws-loop-1".into(),
            owner: owner.into(),
            name: "loop-test".into(),
            region: "centralindia".into(),
            state: WsState::Creating,
            placement: None,
            ref_: None,
            quota_gb: 10,
            live_state: serde_json::Value::Null,
        };
        store.create_ws(&w).await.unwrap();
        let job = rustic_git_workspaces::model::Job {
            id: "job-create-1".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsCreate,
            payload: json!({"workspace": w.id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        };
        store.create_job(&job).await.unwrap();
        w.id
    };

    wait_until(|| async {
        let (w, _) = store.get_ws(owner, &ws_id).await.unwrap().unwrap();
        w.state == WsState::Ready
    })
    .await;

    // A second job (`WsPush`) exercises the loop again and produces an explicit snapshot.
    let job2 = rustic_git_workspaces::model::Job {
        id: "job-push-1".into(),
        region: "centralindia".into(),
        agent: None,
        kind: rustic_git_workspaces::model::JobKind::WsPush,
        payload: json!({"workspace": ws_id, "owner": owner}),
        state: rustic_git_workspaces::model::JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&job2).await.unwrap();

    wait_until(|| async {
        let (w, _) = store.get_ws(owner, &ws_id).await.unwrap().unwrap();
        match &w.ref_ {
            Some(r) => store.get_snapshot(&ws_id, r).await.unwrap().is_some(),
            None => false,
        }
    })
    .await;
}

/// `docker compose version` succeeding is this test's proxy for "docker usable here" — same
/// idea as `have_btrfs()`, skip cleanly rather than fail on a box with neither.
fn have_docker() -> bool {
    std::process::Command::new("docker").args(["compose", "version"]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// EnvUp creates the env's OWN subvolume (no separate workspace) and runs `docker compose up`
/// against volume-folder mounts inside it; EnvDown tears the container down and pushes that one
/// subvolume — a single atomic snapshot covering every mounted volume. Two mounts/two files
/// prove the atomicity: one push, one snapshot record, both files present. Gated on btrfs AND
/// docker.
#[tokio::test]
async fn env_up_writes_into_the_mounts_then_down_pushes_atomically_and_stops() {
    if !have_btrfs() || !have_docker() {
        eprintln!("skipping: btrfs or docker unavailable");
        return;
    }

    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt, HashSet::new());
    state.agent_poll_window = Duration::from_millis(500);
    state.agent_poll_interval = Duration::from_millis(30);
    let state = Arc::new(state);
    store
        .put_region(&Region {
            id: "centralindia".into(),
            name: "Central India".into(),
            storage_account: "acct".into(),
            blob_container: "wslayers".into(),
            status: "active".into(),
            agent_token: TOKEN.into(),
        })
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let base = format!("http://{addr}");

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(Pool::new(lp.pool.root.clone()), blob_store, store.clone() as Arc<dyn MetaStore>));
    let cfg = rustic_git_agent::Config {
        api_url: base.clone(),
        region: "centralindia".into(),
        agent_token: TOKEN.into(),
        pool: lp.pool.root.to_string_lossy().to_string(),
        hostname: "test-agent".into(),
        cpu: 4,
        mem_mb: 16384,
        disk_gb: 128,
    };
    tokio::spawn(rustic_git_agent::run_with_engine(cfg, engine));

    let owner = "bob";

    // An environment owns exactly ONE subvolume; every declared volume is a folder inside it.
    // Two mounts writing two files prove the atomicity claim: EnvDown's one push captures both
    // in a single snapshot, not one push per mounted workspace (there are no mounted
    // workspaces any more).
    use rustic_git_workspaces::model::{Environment, EnvState, Mount, Service};
    let env = Environment {
        id: "env-loop-1".into(),
        owner: owner.into(),
        name: "env-loop-test".into(),
        region: "centralindia".into(),
        state: EnvState::Creating,
        placement: None,
        ref_: None,
        services: vec![Service {
            name: "writer".into(),
            image: "alpine:3".into(),
            command: vec![
                "sh".into(),
                "-c".into(),
                "echo hi > /a/out.txt; echo hi2 > /b/out.txt; sleep 300".into(),
            ],
            env: Default::default(),
            mounts: vec![
                Mount { volume: "data-a".into(), path: "/a".into() },
                Mount { volume: "data-b".into(), path: "/b".into() },
            ],
        }],
    };
    store.create_env(&env).await.unwrap();
    let up_job = rustic_git_workspaces::model::Job {
        id: "job-env-up-1".into(),
        region: "centralindia".into(),
        agent: None,
        kind: rustic_git_workspaces::model::JobKind::EnvUp,
        payload: json!({"environment": env.id, "owner": owner}),
        state: rustic_git_workspaces::model::JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&up_job).await.unwrap();

    wait_until(|| async {
        let (e, _) = store.get_env(owner, &env.id).await.unwrap().unwrap();
        e.state == EnvState::Running
    })
    .await;

    let live = lp.pool.live(&env.id);
    let out_a = live.join("volumes").join("data-a").join("out.txt");
    let out_b = live.join("volumes").join("data-b").join("out.txt");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if out_a.exists() && out_b.exists() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "out.txt never appeared in both mounts");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let down_job = rustic_git_workspaces::model::Job {
        id: "job-env-down-1".into(),
        region: "centralindia".into(),
        agent: None,
        kind: rustic_git_workspaces::model::JobKind::EnvDown,
        payload: json!({"environment": env.id, "owner": owner}),
        state: rustic_git_workspaces::model::JobState::Queued,
        lease_until: None,
        attempts: 0,
        error: None,
    };
    store.create_job(&down_job).await.unwrap();

    wait_until(|| async {
        let (e, _) = store.get_env(owner, &env.id).await.unwrap().unwrap();
        e.state == EnvState::Stopped
    })
    .await;

    let ps = std::process::Command::new("docker")
        .args(["compose", "-p", &format!("env-{}", env.id), "ps", "-q"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&ps.stdout).trim().is_empty(), "container still present after down");

    // EnvDown's push landed the env's OWN ref (not any workspace's), and that one snapshot
    // record — under workspace_id = env id — covers both mounted volumes atomically.
    let (e, _) = store.get_env(owner, &env.id).await.unwrap().unwrap();
    let r = e.ref_.clone().expect("env down should have set the env's own ref");
    let snap = store.get_snapshot(&env.id, &r).await.unwrap().expect("snapshot record under workspace_id = env id");
    assert!(!snap.lineage.is_empty(), "snapshot should have at least one layer");
}

/// Polls `cond` up to 60s — long enough for two agent poll cycles plus the actual btrfs work,
/// or (for the env tests) a cold `docker pull alpine:3`.
async fn wait_until<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if cond().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "condition never became true");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
