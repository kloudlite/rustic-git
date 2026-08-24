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
    // `run_with_engine` wraps its dispatch loop in a `LocalSet` (`WsClone`'s stop/start hooks
    // are `&dyn Fn`, not `Send` — see the doc comment on `run_with_engine`), so the whole
    // future isn't `Send` and can't go through `tokio::spawn`. A dedicated OS thread running
    // its own single-threaded runtime sidesteps that the same way `main()`'s top-level
    // `#[tokio::main]` future does.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(rustic_git_agent::run_with_engine(cfg, engine))
    });

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

/// Polls `cond` up to 5s — long enough for two agent poll cycles plus the actual btrfs work.
async fn wait_until<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if cond().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "condition never became true");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
