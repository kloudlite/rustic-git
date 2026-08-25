//! End-to-end: an in-process server-tier vol-agent router (MemStore, Task 14's
//! `rustic_git_server::vol_agent`) plus a real agent loop (`rustic_git_agent::run_with_engine`)
//! running against a loopback btrfs pool, driven through `WsCreate` then `WsPush`. Gated on
//! `have_btrfs()` — same reason as every other engine test (this Mac, non-root CI).

use object_store::memory::InMemory;
use object_store::ObjectStore;
use rustic_git_workspaces::engine::{have_btrfs, Engine, Pool};
use rustic_git_workspaces::model::{Region, WsState};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Serves the server's vol-agent job routes only (`/vol-agent/register|work|jobs/*`), in-process
/// against `store` (the workspaces `MemStore`), poll window shrunk for the test. The router's
/// state type is `Arc<App>` regardless — the job handlers reach `JobsState` through `Extension`,
/// same as the real `router()` wires it — so a minimal single-node `App` (in-memory object store,
/// nothing under test here needs it to hold real git repos) is what `.with_state` needs.
async fn serve_vol_agent(store: Arc<MemStore>) -> String {
    // Same bearer secret space the agent's engine `RegistryClient` presents to the per-volume
    // `commits`/`ref`/`history` routes (`vol_agent.rs`'s `authorized`) — distinct from the
    // per-region `agent_token` the job routes check, but this test uses one value (`TOKEN`) for
    // both, same as `Config::agent_token` doing double duty in the real agent.
    unsafe { std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", TOKEN) };

    let mut jobs = rustic_git_server::vol_agent::JobsState::new(Some(store as Arc<dyn MetaStore>));
    jobs.poll_window = Duration::from_millis(500);
    jobs.poll_interval = Duration::from_millis(30);

    let tmp = tempfile::tempdir().unwrap();
    let os_store = rustic_git_server::store::Store::open(
        Arc::new(object_store::memory::InMemory::new()),
        tmp.path().join("cache"),
        false,
    )
    .await
    .unwrap();
    let os_store = Arc::new(os_store);
    let ownership = rustic_git_server::ownership::OwnershipStore::open(os_store.os.clone(), true).await.unwrap();
    let app = Arc::new(rustic_git_server::App::new(
        os_store,
        Arc::new(ownership),
        "test-0".into(),
        Arc::new(|_| "127.0.0.1:1".to_string()),
        "test-peer-secret".into(),
        1,
    ));

    // Layer the Extension AFTER both merges, like production's router(): the record routes
    // (commits/ref/history) extract it too, and layering it over only the job routes 500s
    // every record call — the failure mode the first VM run of this harness hit.
    let router = rustic_git_server::vol_agent::vol_agent_job_routes()
        .merge(rustic_git_server::vol_agent::vol_agent_routes())
        .layer(axum::Extension(Arc::new(jobs)))
        .with_state(app);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    // Test-only leak: the cache dir must outlive the spawned server, which outlives this fn's
    // scope; the process exits at test end anyway.
    std::mem::forget(tmp);
    format!("http://{addr}")
}

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
        std::fs::create_dir_all(pool.root.join("vol")).unwrap();
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

    // ── server-tier vol-agent router, in-process, short poll window ──
    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    // ── agent loop, against the SAME MemStore (real deployments point both at the same
    // Cosmos DB) so the engine's `get_ws`/`get_snapshot` calls resolve. ──
    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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
            image: "nginx:alpine".into(),
            placement: None,
            volume: None,
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

    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);
    wait_until(|| async { !registry.get_history(owner, &ws_id).await.unwrap_or_default().is_empty() }).await;
}

/// A `Push` job carrying a message, driven through the agent loop — proves the fused verb lands
/// exactly one new snapshot on the registry, carrying that message, with nothing left unpushed.
#[tokio::test]
async fn push_job_creates_exactly_one_snapshot_with_the_message() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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

    let owner = "carol";
    let ws_id = "ws-loop-commit-push".to_string();
    let w = rustic_git_workspaces::model::Workspace {
        id: ws_id.clone(),
        owner: owner.into(),
        name: "loop-commit-push-test".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&w).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-cp-create".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsCreate,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &ws_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;

    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);
    // `WsCreate` already pushes once (`Engine::init`) — capture that baseline so the assertion
    // below is about what THIS push does, not the create.
    let baseline = registry.get_history(owner, &ws_id).await.unwrap().len();

    std::fs::write(lp.pool.live(&ws_id).join("new.txt"), b"new").unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-push".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::Push,
            payload: json!({"workspace": ws_id, "owner": owner, "message": "checkpoint"}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_job_done(&store, "centralindia", "job-push").await;
    let recs = registry.get_history(owner, &ws_id).await.unwrap();
    assert_eq!(recs.len(), baseline + 1, "exactly one new snapshot");
    assert_eq!(recs[0].message.as_deref(), Some("checkpoint"));
    assert!(lp.pool.lineage(&ws_id).iter().all(|e| !e.unpushed), "nothing left unpushed after a successful push");
}

/// Push variant for an environment: the `Push` job carries `"environment"` (not `"workspace"`)
/// in its payload — the seam this test covers is `run_job` branching on that key to call
/// `engine.push_env` instead of the workspace arm, and the done handler NOT flipping the env's
/// `state` for it (only `EnvUp`/`EnvDown` do that). No docker needed: the env's own subvolume is
/// created directly (`create_subvol`, same call `EnvUp` makes before `compose up`), so this only
/// needs btrfs.
#[tokio::test]
async fn env_push_job_leaves_state_untouched() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));

    let owner = "dana";
    use rustic_git_workspaces::model::{EnvState, Environment};
    let env = Environment {
        id: "env-loop-commit-push".into(),
        owner: owner.into(),
        name: "env-loop-commit-push-test".into(),
        region: "centralindia".into(),
        state: EnvState::Running,
        placement: None,
        volume: None,
        services: vec![],
    };
    store.create_env(&env).await.unwrap();
    // The env's own subvolume, created directly (no EnvUp/docker needed) — same call EnvUp makes
    // before `compose up`.
    engine.create_subvol(&env.id).unwrap();
    std::fs::write(lp.pool.live(&env.id).join("hello.txt"), b"hi").unwrap();

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

    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);

    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-env-push".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::Push,
            payload: json!({"environment": env.id, "owner": owner, "message": "checkpoint"}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_job_done(&store, "centralindia", "job-env-push").await;
    let recs = registry.get_history(owner, &env.id).await.unwrap();
    assert_eq!(recs.len(), 1);
    assert!(!recs[0].lineage.is_empty());
    assert_eq!(recs[0].message.as_deref(), Some("checkpoint"));
    assert!(lp.pool.lineage(&env.id).iter().all(|e| !e.unpushed));
    // Push must not touch env state — only EnvUp/EnvDown do that.
    assert_eq!(store.get_env(owner, &env.id).await.unwrap().unwrap().0.state, EnvState::Running);
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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
        volume: None,
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
                Mount { folder: "data-a".into(), path: "/a".into() },
                Mount { folder: "data-b".into(), path: "/b".into() },
            ],
            ports: vec![],
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

    // EnvDown's commit+push landed the env's OWN registry history (not any workspace's), and
    // that one record — under (owner, env id) — covers both mounted volumes atomically.
    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);
    let recs = registry.get_history(owner, &env.id).await.unwrap();
    assert!(!recs.is_empty(), "env down should have registered at least one commit");
    assert!(!recs[0].lineage.is_empty(), "commit should have at least one layer");
}

/// Environment clone: `env-{src}`'s compose project pauses around a local-first volume clone
/// (`Engine::clone_running_local`/`clone_local_ids`, the same engine paths workspace clone
/// uses), then `dst` is brought up exactly like `EnvUp` would. Proves the file a running
/// service wrote survives the copy and that the SOURCE is still running afterward (the
/// stop/start hooks pause it, never leave it down).
#[tokio::test]
async fn env_clone_copies_content_and_leaves_source_running() {
    if !have_btrfs() || !have_docker() {
        eprintln!("skipping: btrfs or docker unavailable");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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

    let owner = "judy";
    use rustic_git_workspaces::model::{EnvState, Environment, Mount, Service};
    let src_id = "env-loop-clone-src".to_string();
    let services = vec![Service {
        name: "writer".into(),
        image: "alpine:3".into(),
        command: vec!["sh".into(), "-c".into(), "echo from-src > /data/out.txt; sleep 300".into()],
        env: Default::default(),
        mounts: vec![Mount { folder: "data".into(), path: "/data".into() }],
        ports: vec![],
    }];
    let src = Environment {
        id: src_id.clone(),
        owner: owner.into(),
        name: "loop-clone-env-src".into(),
        region: "centralindia".into(),
        state: EnvState::Creating,
        placement: None,
        volume: None,
        services: services.clone(),
    };
    store.create_env(&src).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-env-clone-up".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::EnvUp,
            payload: json!({"environment": src_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_env(owner, &src_id).await.unwrap().unwrap().0.state == EnvState::Running }).await;

    let src_out = lp.pool.live(&src_id).join("volumes").join("data").join("out.txt");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if src_out.exists() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "out.txt never appeared in the source's mount");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let dst_id = "env-loop-clone-dst".to_string();
    let dst = Environment {
        id: dst_id.clone(),
        owner: owner.into(),
        name: "loop-clone-env-dst".into(),
        region: "centralindia".into(),
        state: EnvState::Cloning,
        placement: None,
        volume: None,
        services,
    };
    store.create_env(&dst).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-env-clone".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsClone,
            payload: json!({"environment": dst_id, "src": src_id, "owner": owner, "stop_project": format!("env-{src_id}")}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_env(owner, &dst_id).await.unwrap().unwrap().0.state == EnvState::Running }).await;

    let dst_out = lp.pool.live(&dst_id).join("volumes").join("data").join("out.txt");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if dst_out.exists() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "out.txt never appeared in the clone's mount");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(std::fs::read_to_string(&dst_out).unwrap().trim(), "from-src");

    // The source must still be running: the stop/start hooks pause its compose project around
    // the copy, never leave it down.
    let ps = std::process::Command::new("docker")
        .args(["compose", "-p", &format!("env-{src_id}"), "ps", "-q"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&ps.stdout).trim().is_empty(), "source container(s) not running after clone");
}

/// `WsCreate` starts `ws-{id}` (default image `nginx:alpine`) with the live subvolume double
/// bind-mounted; `WsStop`/`WsStart` move the container (and the doc's state) between
/// stopped/running. Gated on btrfs AND docker like the env container test above.
#[tokio::test]
async fn ws_create_runs_a_container_then_stop_and_start_toggle_it() {
    if !have_btrfs() || !have_docker() {
        eprintln!("skipping: btrfs or docker unavailable");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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

    let owner = "erin";
    let ws_id = "ws-loop-container".to_string();
    let cname = format!("ws-{ws_id}");
    // Cleaned even if an assertion below panics mid-test.
    struct RmGuard(String);
    impl Drop for RmGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let _rm_guard = RmGuard(cname.clone());

    let w = rustic_git_workspaces::model::Workspace {
        id: ws_id.clone(),
        owner: owner.into(),
        name: "loop-container-test".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&w).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-container-create".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsCreate,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &ws_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;

    let ps = std::process::Command::new("docker")
        .args(["ps", "--filter", &format!("name={cname}"), "--format", "{{.Names}}"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&ps.stdout).trim(), cname, "container not running after WsCreate");

    // The live subvolume's files are visible inside the container at /usr/share/nginx/html — the
    // default image's web root, deliberately double-mounted alongside /workspace (see
    // bins/agent/src/container.rs). `hello.txt` is written directly by `Engine::init`.
    let ls = std::process::Command::new("docker").args(["exec", &cname, "ls", "/usr/share/nginx/html"]).output().unwrap();
    assert!(ls.status.success(), "docker exec ls failed: {}", String::from_utf8_lossy(&ls.stderr));

    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-container-stop".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsStop,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &ws_id).await.unwrap().unwrap().0.state == WsState::Stopped }).await;
    wait_until(|| async {
        let ps = std::process::Command::new("docker")
            .args(["ps", "--filter", &format!("name={cname}"), "--format", "{{.Names}}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim().is_empty()
    })
    .await;

    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-container-start".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsStart,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &ws_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;
    wait_until(|| async {
        let ps = std::process::Command::new("docker")
            .args(["ps", "--filter", &format!("name={cname}"), "--format", "{{.Names}}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim() == cname
    })
    .await;
}

/// `WsDelete` must reclaim everything local: the `{pool}/vol/{id}` directory itself, and every RO
/// snapshot its lineage named (not just the `live` subvolume) — the completed-delete half of the
/// storage-hygiene work (registry/blob bytes stay put, untouched by design). Gated on btrfs AND
/// docker like the other container-driving loop tests.
#[tokio::test]
async fn ws_delete_reclaims_the_local_volume_directory() {
    if !have_btrfs() || !have_docker() {
        eprintln!("skipping: btrfs or docker unavailable");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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

    let owner = "frank";
    let ws_id = "ws-loop-delete".to_string();
    let cname = format!("ws-{ws_id}");
    struct RmGuard(String);
    impl Drop for RmGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let _rm_guard = RmGuard(cname.clone());

    let w = rustic_git_workspaces::model::Workspace {
        id: ws_id.clone(),
        owner: owner.into(),
        name: "loop-delete-test".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&w).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-delete-create".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsCreate,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &ws_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;

    // `WsCreate` -> `Engine::init` already committed+pushed once, so there's at least one pushed
    // snapshot on the pool before delete — the assertion below proves delete reclaims it, not
    // just an empty lineage.
    let lineage_before = lp.pool.lineage(&ws_id);
    assert!(!lineage_before.is_empty(), "init should have left at least one lineage entry");
    let snap_root = lp.pool.snap_root(&ws_id);

    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-delete-delete".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsDelete,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();

    let voldir = lp.pool.voldir(&ws_id);
    wait_until(|| async { !voldir.exists() }).await;

    for e in &lineage_before {
        assert!(!snap_root.join(e.snap_name()).exists(), "snapshot {} should have been reclaimed", e.snap_name());
    }

    // `WsDelete` already removed the container (`container::remove`); a `WsStop` racing in after
    // it — the same "no such container" `docker stop` would hit — must still complete instead of
    // failing/retrying forever (`container::stop`'s absence tolerance, same shape `remove`
    // already had).
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-delete-stop-absent".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsStop,
            payload: json!({"workspace": ws_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_job_done(&store, "centralindia", "job-delete-stop-absent").await;
}

/// Proves the stage-file sharing `Engine::clone_local_snapshot`'s doc promises: clone a source
/// that has an UNPUSHED commit (so the shared stage file actually matters), then delete the
/// source. `dst`'s eventual push must still succeed — `cleanup_local`'s stage-file removal must
/// have skipped the blob because `dst`'s own lineage still references it (`other_unpushed_blobs`
/// in `bins/agent/src/lib.rs`), even though the WsDelete job only names the source.
#[tokio::test]
async fn ws_delete_of_cloned_source_leaves_dst_pushable() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store.clone(),
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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
    tokio::spawn(rustic_git_agent::run_with_engine(cfg, engine.clone()));

    let owner = "heidi";
    let src_id = "ws-loop-clone-del-src".to_string();
    let src = rustic_git_workspaces::model::Workspace {
        id: src_id.clone(),
        owner: owner.into(),
        name: "loop-clone-del-src".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&src).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-fd-create".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsCreate,
            payload: json!({"workspace": src_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &src_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;

    // Nothing user-facing can leave an unpushed mark any more (`push` is the one atomic verb),
    // so this test manufactures the crash-recovery window directly: a push against an
    // unreachable registry stages the layer locally and fails before the registry call lands,
    // leaving exactly the internal state a crashed push would — the shared stage file whose
    // survival across clone + source deletion is what this test is actually about.
    std::fs::write(lp.pool.live(&src_id).join("unpushed.txt"), b"unpushed").unwrap();
    let (src_doc, _) = store.get_ws(owner, &src_id).await.unwrap().unwrap();
    let broken = Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store.clone(),
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new("http://127.0.0.1:1", TOKEN),
    );
    assert!(broken.push(&src_doc, None).await.is_err(), "unreachable registry must fail the push");
    assert!(lp.pool.lineage(&src_id).iter().any(|e| e.unpushed));

    let dst_id = "ws-loop-clone-del-dst".to_string();
    let dst = rustic_git_workspaces::model::Workspace {
        id: dst_id.clone(),
        owner: owner.into(),
        name: "loop-clone-del-dst".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&dst).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-fd-clone".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsClone,
            // No `stop_container` running (the source was never started via `container::start`
            // here), so the agent's `is_running` check is false and it goes `clone_local`.
            payload: json!({"workspace": dst_id, "owner": owner, "src": src_id}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { lp.pool.live(&dst_id).exists() }).await;
    // Clone must have gone local-first: no registry call, so `dst` has no history of its own yet.
    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);
    assert!(registry.get_history(owner, &dst_id).await.unwrap().is_empty());
    assert!(lp.pool.lineage(&dst_id).iter().any(|e| e.unpushed), "dst must inherit the unpushed mark verbatim");

    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-fd-delete".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsDelete,
            payload: json!({"workspace": src_id, "owner": owner}),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { !lp.pool.voldir(&src_id).exists() }).await;

    // The source (and its stage files, if the delete-skip rule didn't hold) is gone; dst's push
    // must still succeed, proving the shared stage file survived.
    let (dst_ws, _) = store.get_ws(owner, &dst_id).await.unwrap().unwrap();
    engine.push(&dst_ws, None).await.unwrap();
    assert!(!registry.get_history(owner, &dst_id).await.unwrap().is_empty());
}

/// Cloning a RUNNING source that has never pushed (zero registry history) used to fail with
/// "clone source has no snapshots; push first" — `clone_running`'s only path was the
/// registry-prefetch one, which needs `inherit` to find at least one `CommitRecord`.
/// `clone_running_local` fixes that: it snapshots the source's LIVE subvolume directly (no
/// registry call at all), so it works with zero history and — the point of stop/sync/snapshot,
/// proven here — captures a write made after the source's own last (nonexistent) snapshot.
/// Docker- and btrfs-gated: the source's container has to genuinely be running for the agent to
/// pick `clone_running` over `clone_local`.
#[tokio::test]
async fn clone_of_running_never_pushed_source_captures_live_content() {
    if !have_btrfs() || !have_docker() {
        eprintln!("skipping: btrfs or docker unavailable");
        return;
    }

    let store = Arc::new(MemStore::new());
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
    let base = serve_vol_agent(store.clone()).await;

    let lp = LoopbackPool::new();
    let blob_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let engine = Arc::new(Engine::new(
        Pool::new(lp.pool.root.clone()),
        blob_store,
        store.clone() as Arc<dyn MetaStore>,
        rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN),
    ));
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
    tokio::spawn(rustic_git_agent::run_with_engine(cfg, engine.clone()));

    let owner = "ivan";
    let src_id = "ws-loop-clone-running-src".to_string();
    let dst_id = "ws-loop-clone-running-dst".to_string();
    let src_cname = format!("ws-{src_id}");
    let dst_cname = format!("ws-{dst_id}");
    struct RmGuard(Vec<String>);
    impl Drop for RmGuard {
        fn drop(&mut self) {
            for c in &self.0 {
                let _ = std::process::Command::new("docker").args(["rm", "-f", c]).output();
            }
        }
    }
    let _rm_guard = RmGuard(vec![src_cname.clone(), dst_cname.clone()]);

    let src = rustic_git_workspaces::model::Workspace {
        id: src_id.clone(),
        owner: owner.into(),
        name: "loop-clone-running-src".into(),
        region: "centralindia".into(),
        state: WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&src).await.unwrap();
    // Bypass `WsCreate`/`Engine::init` (which pushes immediately) so the source genuinely has
    // ZERO registry history — a live subvolume with a container running against it, never
    // pushed even once.
    engine.create_subvol(&src.id).unwrap();
    std::fs::write(engine.pool.live(&src.id).join("before.txt"), b"before").unwrap();
    rustic_git_agent::container::start(&src.id, &src.image, &engine.pool.live(&src.id)).unwrap();
    wait_until(|| async {
        let ps = std::process::Command::new("docker")
            .args(["ps", "--filter", &format!("name={src_cname}"), "--format", "{{.Names}}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim() == src_cname
    })
    .await;

    // Written after the container started, with no snapshot taken since — a registry-prefetch
    // path (which only ever sees pushed content) could never see this; only a live capture can.
    std::fs::write(engine.pool.live(&src.id).join("after.txt"), b"after").unwrap();

    let dst = rustic_git_workspaces::model::Workspace {
        id: dst_id.clone(),
        owner: owner.into(),
        name: "loop-clone-running-dst".into(),
        region: "centralindia".into(),
        state: WsState::Creating,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 10,
        live_state: serde_json::Value::Null,
    };
    store.create_ws(&dst).await.unwrap();
    store
        .create_job(&rustic_git_workspaces::model::Job {
            id: "job-clone-running".into(),
            region: "centralindia".into(),
            agent: None,
            kind: rustic_git_workspaces::model::JobKind::WsClone,
            payload: json!({
                "workspace": dst_id, "src": src_id, "owner": owner,
                "stop_container": src_cname, "stop_projects": [],
            }),
            state: rustic_git_workspaces::model::JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        })
        .await
        .unwrap();
    wait_until(|| async { store.get_ws(owner, &dst_id).await.unwrap().unwrap().0.state == WsState::Ready }).await;

    assert_eq!(std::fs::read(engine.pool.live(&dst_id).join("before.txt")).unwrap(), b"before");
    assert_eq!(
        std::fs::read(engine.pool.live(&dst_id).join("after.txt")).unwrap(),
        b"after",
        "live capture must include a write made after the source's (nonexistent) last snapshot"
    );

    // Local-first, no network: dst inherits src's (empty) lineage verbatim, no registry history.
    assert!(lp.pool.lineage(&dst_id).is_empty());
    let registry = rustic_git_workspaces::registry_client::RegistryClient::new(&base, TOKEN);
    assert!(registry.get_history(owner, &dst_id).await.unwrap().is_empty());

    // The source's container must be running again after the stop/snapshot/start window.
    wait_until(|| async {
        let ps = std::process::Command::new("docker")
            .args(["ps", "--filter", &format!("name={src_cname}"), "--format", "{{.Names}}"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim() == src_cname
    })
    .await;
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

/// Waits for a job to leave `Queued`/`Leased`, then asserts it landed on `Done` — panicking with
/// the job's own recorded `error` if it went to `Failed` instead. A test that only inferred
/// success from a side effect (a registry record appearing) can pass while the job is still
/// mid-flight: `post_commits` lands before the ref move and the local unpushed-mark cleanup that
/// follow it, so history-non-empty is not "the push finished". Checking the job doc itself is
/// race-free (the agent's `report()` call that flips it to `Done` only fires after `run_job`
/// returns, which is after every local write) and turns a silently-swallowed engine error into a
/// loud test failure instead of a flaky assertion somewhere downstream.
async fn wait_job_done(store: &MemStore, region: &str, job_id: &str) {
    use rustic_git_workspaces::model::JobState;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some((job, _)) = store.get_job(region, job_id).await.unwrap() {
            match job.state {
                JobState::Done => return,
                JobState::Failed => panic!("job {job_id} failed: {}", job.error.unwrap_or_default()),
                JobState::Queued | JobState::Leased => {}
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "job {job_id} never left queued/leased");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
