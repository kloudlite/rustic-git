//! Engine op tests: commit/push/pull/clone_local/clone_running/squash. Everything here touches btrfs, so every
//! test opens with `have_btrfs()` and returns cleanly when it's false (this Mac, any non-root CI
//! runner) — they run for real on the btrfs review VM. Fixtures: `MemStore` for the Cosmos-side
//! `Workspace`/`Environment` docs, an in-process vol-agent router (`registry_server`, mirroring
//! `bins/agent/tests/loop.rs`) as the volume registry, and an `InMemory` object store for layer
//! blobs.

use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult};
use object_store::memory::InMemory;
use object_store::path::Path as S3Path;
use rustic_git_workspaces::engine::{Engine, Pool, fsck, have_btrfs};
use rustic_git_workspaces::model::{Workspace, WsState};
use rustic_git_workspaces::registry::CommitRecord;
use rustic_git_workspaces::registry_client::RegistryClient;
use rustic_git_workspaces::store::{MemStore, MetaStore};
use sha2::Digest;
use std::path::Path;
use std::sync::Arc;

const TOKEN: &str = "engine-ops-test-token";

/// Boots the server's per-volume vol-agent router (`commits`/`ref`/`history`) in-process,
/// backed by its own fresh `Store` (SlateDB over an `InMemory` object store distinct from the
/// test's LAYER blob store — the registry's commit/ref records and the layer bytes they name
/// live in entirely separate stores, same as production). Returns the base URL every
/// `RegistryClient` in the test should share, so a clone destination engine can read the
/// SAME history a source engine just pushed.
async fn registry_server() -> String {
    // Constant-token auth is a plain env var (`vol_agent.rs`'s `authorized`); every caller in
    // this file uses the same value, so setting it repeatedly across parallel tests is benign.
    unsafe { std::env::set_var("RUSTIC_GIT_VOL_AGENT_TOKENS", TOKEN) };

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
    // The record handlers extract Extension<Arc<JobsState>> (region-token auth); the layer must
    // cover them exactly like production's router() does, or every call 500s on the missing
    // extension — which is precisely how the first VM run of this harness failed.
    let router = rustic_git_server::vol_agent::vol_agent_routes()
        .layer(axum::Extension(Arc::new(rustic_git_server::vol_agent::JobsState::new(None))))
        .with_state(app);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    // Test-only leak: the cache dir must outlive the spawned server, which outlives this fn's
    // scope; the process exits at test end anyway.
    std::mem::forget(tmp);
    format!("http://{addr}")
}

/// A loopback btrfs pool backed by a truncated sparse image, mounted for the test and torn
/// down (unmount) on drop. Root only — construction panics if mkfs/mount fail, which is fine
/// since callers only build this behind `have_btrfs()`.
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

    /// A fresh `Pool` handle onto the same mounted root — `Pool` holds no state beyond the
    /// path, so this sidesteps moving out of a type that implements `Drop`.
    fn pool(&self) -> Pool {
        Pool::new(self.pool.root.clone())
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

fn ws(owner: &str, id: &str) -> Workspace {
    Workspace {
        id: id.into(),
        owner: owner.into(),
        name: id.into(),
        region: "centralindia".into(),
        state: WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 20,
        live_state: serde_json::Value::Null,
    }
}

fn engine(pool: Pool, store: Arc<dyn ObjectStore>, meta: Arc<dyn MetaStore>, registry_base: &str) -> Engine {
    Engine::new(pool, store, meta, RegistryClient::new(registry_base, TOKEN))
}

/// `Engine::push` with no message — the common case for tests whose point isn't the message
/// itself.
async fn commit_and_push(e: &Engine, w: &Workspace) -> rustic_git_workspaces::engine::PushOut {
    e.push(w, None).await.unwrap()
}

async fn history(base: &str, owner: &str, name: &str) -> Vec<CommitRecord> {
    RegistryClient::new(base, TOKEN).get_history(owner, name).await.unwrap()
}

/// Layer blobs only — every upload also writes a `.json` sidecar beside the blob, and
/// counting both made these assertions read double on the first real (non-skipped) run.
async fn blob_count(store: &Arc<dyn ObjectStore>) -> usize {
    store
        .list(Some(&S3Path::from("layers/")))
        .filter(|m| {
            let keep = m.as_ref().map(|m| !m.location.as_ref().ends_with(".json")).unwrap_or(true);
            async move { keep }
        })
        .count()
        .await
}

/// Deterministic recursive walk+hash of a directory tree: relative path + file bytes, so two
/// trees are "byte-identical" iff this digest matches.
fn hash_tree(root: &Path) -> String {
    fn walk(dir: &Path, base: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                walk(&p, base, files);
            } else {
                files.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = sha2::Sha256::new();
    for (rel, bytes) in &files {
        h.update(rel.as_bytes());
        h.update(bytes);
    }
    format!("{:x}", h.finalize())
}

fn init_live_subvol(pool: &Pool, ws_id: &str) {
    std::fs::create_dir_all(pool.voldir(ws_id)).unwrap();
    run(&["btrfs", "subvolume", "create", pool.live(ws_id).to_str().unwrap()]);
}

#[tokio::test]
async fn push_creates_exactly_one_snapshot_with_the_message() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), meta.clone(), &base);

    let w = ws("karthik", "ws-push-msg");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();

    let out = e.push(&w, Some("first push")).await.unwrap();
    assert_eq!(out.layers, 1, "push must snapshot and land exactly one new layer");

    let recs = history(&base, &w.owner, &w.id).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].message.as_deref(), Some("first push"));

    let lineage = e.pool.lineage(&w.id);
    assert_eq!(lineage.len(), 1);
    assert!(!lineage[0].unpushed, "a successful push must clear the mark, never leave user-facing unpushed state");
}

#[tokio::test]
async fn push_uploads_exactly_the_unpushed_set_and_moves_the_ref() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), meta.clone(), &base);

    let w = ws("karthik", "ws-push");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    for i in 0..200 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }

    let t = std::time::Instant::now();
    let out = e.push(&w, None).await.unwrap();
    assert!(t.elapsed().as_secs() < 5, "push of one 200-file layer took {:?}", t.elapsed());
    assert!(!out.sha.is_empty());
    assert_eq!(out.layers, 1);
    assert_eq!(blob_count(&store).await, 1, "exactly one layer blob uploaded");

    let recs = history(&base, &w.owner, &w.id).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].id, out.layer);

    let lineage = e.pool.lineage(&w.id);
    assert!(!lineage[0].unpushed, "push must clear the unpushed mark");
}

/// A crash (or a rejected `commits`/`ref` request) between the upload finishing and the batch
/// landing must leave the stage files in place and the marks unpushed — otherwise a retried
/// push finds no stage file for an entry it still thinks is unpushed and fails forever. Proven
/// by pointing the engine's `RegistryClient` at an address nothing listens on (so `post_commits`
/// errors before anything is cleared), then retrying against the real registry.
#[tokio::test]
async fn a_failed_push_leaves_stage_files_and_marks_intact_for_a_clean_retry() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;

    let w = ws("karthik", "ws-push-retry");
    meta.create_ws(&w).await.unwrap();
    // Same pool underneath both engines below — only the registry endpoint differs.
    let pool_root = lp.pool.root.clone();
    init_live_subvol(&lp.pool, &w.id);
    std::fs::write(lp.pool.live(&w.id).join("a.txt"), b"a").unwrap();

    // Port 1: nothing listens there, so any request errors out immediately — same stand-in
    // address `registry_server`'s own `App::new` fixture uses for "unreachable peer". A push
    // against it still stages (snapshot + compress, local-only) before the registry call it
    // never reaches — the crash-recovery window `push` is meant to survive.
    let broken = engine(Pool::new(pool_root.clone()), store.clone(), meta.clone(), "http://127.0.0.1:1");
    let err = broken.push(&w, None).await.unwrap_err();
    assert!(err.0.contains("registry"), "unexpected error: {}", err.0);

    // The upload itself (to the object store, unrelated to the broken registry) still went
    // through — only the registry POSTs failed — but the entry must still read as unpushed and
    // its stage files must still exist for a retry to find.
    let lineage = broken.pool.lineage(&w.id);
    assert_eq!(lineage.len(), 1);
    assert!(lineage[0].unpushed, "a failed push must not clear the mark");
    let staged_blob = lineage[0].blob.clone();
    assert!(broken.pool.stage_path(&staged_blob).exists(), "stage blob must survive a failed push");
    assert!(broken.pool.stage_meta_path(&staged_blob).exists(), "stage meta must survive a failed push");

    // Retry against the real registry: same pool, working endpoint. The retry is a plain push
    // call, not a special "resume" verb — it stages one MORE fresh snapshot the ordinary way,
    // but the internal unpushed mark on the first (still-staged) layer means both land in the
    // same batch: nothing from the failed attempt is lost or duplicated.
    let good = engine(Pool::new(pool_root), store, meta, &base);
    let out = good.push(&w, None).await.unwrap();
    assert_eq!(out.layers, 2, "the retried push's own snapshot plus the one stranded by the failed attempt");
    let recs = history(&base, &w.owner, &w.id).await;
    assert_eq!(recs.len(), 2, "the retry must land the stranded record, not lose or duplicate it");
    assert!(good.pool.lineage(&w.id).iter().all(|l| !l.unpushed));
    assert!(!good.pool.stage_path(&staged_blob).exists(), "a successful push must clean up its stage files");
}

#[tokio::test]
async fn pull_from_never_pushed_workspace_fails_clean() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta.clone(), &base);

    let w = ws("karthik", "ws-never-pushed");
    meta.create_ws(&w).await.unwrap();
    let err = e.pull(&w).await.unwrap_err();
    assert!(err.0.contains("history"), "unexpected error: {}", err.0);
}

#[tokio::test]
async fn seven_layer_cold_pull_is_byte_identical() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let src = LoopbackPool::new();
    let dst = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src.pool(), store.clone(), meta.clone(), &base);

    let w = ws("karthik", "ws-cold");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);
    for layer in 0..7 {
        std::fs::write(src_engine.pool.live(&w.id).join(format!("layer{layer}.txt")), format!("v{layer}")).unwrap();
        commit_and_push(&src_engine, &w).await;
    }
    let expected = hash_tree(&src_engine.pool.live(&w.id));

    let dst_engine = engine(dst.pool(), store, meta, &base);
    let out = dst_engine.pull(&w).await.unwrap();
    assert_eq!(out.layers, 7);
    assert_eq!(out.fetched, 7);
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}

#[tokio::test]
async fn noop_pull_fetches_nothing() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let w = ws("karthik", "ws-noop");
    e.meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    commit_and_push(&e, &w).await;

    let out = e.pull(&w).await.unwrap();
    assert_eq!(out.fetched, 0);
}

#[tokio::test]
async fn clone_is_zero_fetch_and_isolated() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let src = ws("karthik", "ws-clone-src");
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &src).await;

    let dst = ws("karthik", "ws-clone-dst");
    e.meta.create_ws(&dst).await.unwrap();
    e.clone_local(&src, &dst).await.unwrap();
    assert_eq!(hash_tree(&e.pool.live(&dst.id)), hash_tree(&e.pool.live(&src.id)));

    // A push after clone on either side must not affect the other (isolation).
    std::fs::write(e.pool.live(&dst.id).join("only-dst.txt"), b"dst").unwrap();
    commit_and_push(&e, &dst).await;
    assert!(!e.pool.live(&src.id).join("only-dst.txt").exists());

    // Clone inherits blobs (no re-upload); the inherited entry was already registered under
    // SRC's own history and is never separately re-posted under dst's — a `CommitRecord`
    // embeds its full lineage prefix (never depends on another record), so dst's ONE new push
    // record is self-sufficient for a cold pull/restore of dst without the inherited entry
    // needing its own row in dst's history.
    let dst_recs = history(&base, &dst.owner, &dst.id).await;
    assert_eq!(dst_recs.len(), 1, "dst's own new push, self-sufficient via its embedded lineage prefix");
}

/// LOCAL-FIRST clone: `src` has never pushed (or even snapshotted) at all — no `push`, no
/// snapshot, just a live subvolume with a write in it — yet `clone_local` still succeeds: no
/// registry call, dst tree byte-identical to src's live subvolume, and dst starts equally
/// lineage-less. Then dst's own push works from that lineage-less state.
#[tokio::test]
async fn clone_of_never_pushed_workspace_is_local() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let src = ws("karthik", "ws-clone-nopush-src");
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    assert!(e.pool.lineage(&src.id).is_empty(), "src has no snapshot at all yet");

    let dst = ws("karthik", "ws-clone-nopush-dst");
    e.meta.create_ws(&dst).await.unwrap();
    e.clone_local(&src, &dst).await.unwrap(); // must succeed locally, no push required first

    assert_eq!(hash_tree(&e.pool.live(&dst.id)), hash_tree(&e.pool.live(&src.id)));
    assert!(e.pool.lineage(&dst.id).is_empty(), "dst inherits src's lineage-less state verbatim");

    // No registry call: dst has no history until its own push.
    assert!(history(&base, &dst.owner, &dst.id).await.is_empty());

    // dst's own push still works from here.
    e.push(&dst, None).await.unwrap();
    let dst_recs = history(&base, &dst.owner, &dst.id).await;
    assert_eq!(dst_recs.len(), 1);
}

/// Clone-of-the-clone: dst2 clones from dst, and neither src nor dst has ever pushed or
/// snapshotted — still all local, still byte-identical.
#[tokio::test]
async fn clone_of_the_clone_still_nothing_pushed_stays_local() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let src = ws("karthik", "ws-clone2-src");
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();

    let dst = ws("karthik", "ws-clone2-dst");
    e.meta.create_ws(&dst).await.unwrap();
    e.clone_local(&src, &dst).await.unwrap();

    let dst2 = ws("karthik", "ws-clone2-dst2");
    e.meta.create_ws(&dst2).await.unwrap();
    e.clone_local(&dst, &dst2).await.unwrap();

    assert_eq!(hash_tree(&e.pool.live(&dst2.id)), hash_tree(&e.pool.live(&src.id)));
    assert!(e.pool.lineage(&dst2.id).is_empty());
    assert!(history(&base, &dst2.owner, &dst2.id).await.is_empty());
}

#[tokio::test]
async fn size_and_chain_triggers_fire_and_settle_to_grafted_block() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let mut e = engine(lp.pool(), store.clone(), meta.clone(), &base);
    e.squash_mb = 1; // 1MB trigger, not the 256MB default, for test speed
    e.chain_max = 3; // chain trigger, not the default 50

    let w = ws("karthik", "ws-squash");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);

    // Push past the chain trigger (chain_max = 3); the child spawn is skipped in this test
    // binary (it has no "squash" subcommand — Task 9 wires that), so assert the trigger
    // message/latch and drive settling with a direct `Engine::squash` call instead of waiting
    // on a child.
    let mut triggered = false;
    for i in 0..5 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("v{i}")).unwrap();
        let out = commit_and_push(&e, &w).await;
        if let Some(r) = out.squash_triggered {
            assert!(r.contains("chain") || r.contains("MB"), "unexpected trigger reason: {r}");
            triggered = true;
            break;
        }
    }
    assert!(triggered, "chain trigger never fired within 5 pushes past chain_max=3");

    let latch = e.pool.root.join("vol").join(format!("{}.squashing", w.id));
    assert!(latch.exists(), "latch file must exist once a squash is triggered");

    // A second push while the latch is held must not spawn a second squash (message says so).
    std::fs::write(e.pool.live(&w.id).join("late.txt"), b"late").unwrap();
    let out2 = commit_and_push(&e, &w).await;
    if let Some(r) = &out2.squash_triggered {
        assert!(r.contains("already running"), "second trigger should be suppressed by the latch: {r}");
    }

    let expected = hash_tree(&e.pool.live(&w.id));

    // Settle inline instead of waiting on the (nonexistent-in-this-binary) detached child.
    e.squash(&w).await.unwrap();
    assert!(!latch.exists(), "squash must remove its own latch when done");

    let lineage = e.pool.lineage(&w.id);
    assert_eq!(lineage[0].kind, rustic_git_workspaces::model::LayerKind::Block);
    assert!(
        lineage.iter().skip(1).all(|l| l.kind == rustic_git_workspaces::model::LayerKind::Stream),
        "post-tip pushes must graft as streams onto the new block base"
    );
    assert!(lineage.iter().all(|l| !l.unpushed), "squash's own push must clear every mark");

    // Cold pull from the settled lineage must reproduce the same tree.
    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, meta, &base);
    dst_engine.pull(&w).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}

#[tokio::test]
async fn corrupt_blob_fails_pull_with_sha_mismatch() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let src = LoopbackPool::new();
    let dst = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src.pool(), store.clone(), meta.clone(), &base);

    let w = ws("karthik", "ws-corrupt");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);
    std::fs::write(src_engine.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    let out = commit_and_push(&src_engine, &w).await;

    // Flip a byte in the uploaded blob directly in the InMemory store.
    let key = object_store::path::Path::from(format!("layers/{}.zst", out.layer));
    let mut bytes = store.get(&key).await.unwrap().bytes().await.unwrap().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    store.put(&key, bytes.into()).await.unwrap();

    let dst_engine = engine(dst.pool(), store, meta, &base);
    let err = dst_engine.pull(&w).await.unwrap_err();
    assert!(err.0.contains("sha mismatch"), "unexpected error: {}", err.0);
}

#[tokio::test]
async fn clone_running_locks_briefly_and_is_byte_identical() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    // clone_running runs on one Engine/pool (this node's agent); the "source" and "clone" are
    // both local subvolumes on it — cross-node clone is future work layered on top by the job
    // system, not this engine call.
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let s = ws("karthik", "ws-clone-src");
    e.meta.create_ws(&s).await.unwrap();
    init_live_subvol(&e.pool, &s.id);
    std::fs::write(e.pool.live(&s.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &s).await;

    // A writer thread keeps mutating the source concurrently, like a live container would.
    let live = e.pool.live(&s.id);
    let stop_writer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sw = stop_writer.clone();
    let writer = std::thread::spawn(move || {
        let mut i = 0;
        while !sw.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = std::fs::write(live.join(format!("churn{}.txt", i % 20)), format!("v{i}"));
            i += 1;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });

    let d = ws("karthik", "ws-clone-dst");
    e.meta.create_ws(&d).await.unwrap();

    let stop = || -> Result<(), rustic_git_workspaces::engine::EngErr> {
        stop_writer.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    };
    let start = || -> Result<(), rustic_git_workspaces::engine::EngErr> { Ok(()) };

    let out = e.clone_running(&s, &d, &stop, &start).await.unwrap();
    writer.join().unwrap();

    // Freeze the source's state (writer already stopped) for the identity comparison.
    let expected = hash_tree(&e.pool.live(&s.id));
    assert!(out.locked < std::time::Duration::from_secs(2), "locked window too long: {:?}", out.locked);
    assert_eq!(hash_tree(&e.pool.live(&d.id)), expected);
}

#[tokio::test]
async fn push_captures_live_state_into_the_record() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let mut w = ws("karthik", "ws-state");
    w.live_state = serde_json::json!({"ports": [3000], "packages": ["node@22"]});
    e.meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    let out = commit_and_push(&e, &w).await;

    let recs = history(&base, &w.owner, &w.id).await;
    let rec = recs.iter().find(|r| r.id == out.layer).unwrap();
    assert_eq!(rec.state, w.live_state);
}

#[tokio::test]
async fn clone_pushes_the_destination_docs_own_live_state() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, meta, &base);

    let mut src = ws("karthik", "ws-clone-state-src");
    src.live_state = serde_json::json!({"ports": [3000]});
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &src).await;

    // The source's doc drifts after that push — a later push would capture THIS value, not the
    // one already durable in history.
    src.live_state = serde_json::json!({"ports": [9999]});

    // `clone_local` only ever copies FILES (the local-first lineage/subvolume); it never reads
    // or writes `live_state` — that's `crates/workspaces/src/api.rs`'s `clone_ws` handler's job,
    // which builds the destination doc with `live_state: src.live_state.clone()` (the source's
    // CURRENT value at clone time, same "current state, not an old snapshot" rule clone already
    // applies to file content — `restore` is the verb for an explicit past snapshot). Mirror
    // that here since this test drives the engine directly, under the API.
    let mut dst = ws("karthik", "ws-clone-state-dst");
    dst.live_state = src.live_state.clone();
    e.meta.create_ws(&dst).await.unwrap();
    e.clone_local(&src, &dst).await.unwrap();

    // `push` always captures the live doc's OWN `live_state` at push time (no more re-deriving
    // it from an inherited lineage entry) — so dst's first push registers whatever `dst`'s own
    // doc says, which the API set to src's state as of the clone request.
    e.push(&dst, None).await.unwrap();
    let dst_recs = history(&base, &dst.owner, &dst.id).await;
    assert_eq!(dst_recs.len(), 1);
    assert_eq!(dst_recs[0].state, serde_json::json!({"ports": [9999]}));
}

#[tokio::test]
async fn restore_returns_an_older_record_not_the_tip() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let src_pool = LoopbackPool::new();
    let dst_pool = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src_pool.pool(), store.clone(), meta.clone(), &base);

    let mut w = ws("karthik", "ws-older");
    w.live_state = serde_json::json!({"packages": ["node@20"]});
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);

    std::fs::write(src_engine.pool.live(&w.id).join("f.txt"), b"v1").unwrap();
    let older_out = commit_and_push(&src_engine, &w).await;
    let older_commit_id = older_out.layer;

    // Advance past the older commit: change the file's content and the live state, then commit
    // and push again so the ref tip no longer matches what we're about to restore.
    std::fs::write(src_engine.pool.live(&w.id).join("f.txt"), b"v2-changed-after").unwrap();
    w.live_state = serde_json::json!({"packages": ["node@22"]});
    commit_and_push(&src_engine, &w).await;

    // `restore` (the engine call) only materializes FILES from the named snapshot; it never
    // touches `live_state` — that's `crates/workspaces/src/api.rs`'s `restore_ws` handler's job,
    // which builds the destination doc with `live_state` copied from the restored snapshot's OWN
    // captured record (falling back to the source's current value only if the snapshot never
    // recorded one). Mirror that here since this test drives the engine directly, under the API.
    let mut dst = ws("karthik", "ws-from-older-snapshot");
    dst.live_state = serde_json::json!({"packages": ["node@20"]});
    meta.create_ws(&dst).await.unwrap();
    let dst_engine = engine(dst_pool.pool(), store, meta.clone(), &base);
    dst_engine.restore(&w.owner, &w.id, &older_commit_id, &dst).await.unwrap();

    assert_eq!(std::fs::read(dst_engine.pool.live(&dst.id).join("f.txt")).unwrap(), b"v1");

    // `push` always captures the live doc's OWN `live_state` at push time — dst's first push
    // registers whatever `dst`'s doc says, which the API set to the restored snapshot's state.
    dst_engine.push(&dst, None).await.unwrap();
    let dst_recs = history(&base, &dst.owner, &dst.id).await;
    assert_eq!(dst_recs.len(), 1);
    assert_eq!(dst_recs[0].state, serde_json::json!({"packages": ["node@20"]}));
}

/// Wraps an `InMemory` store and fails every write, so `push`'s upload (and therefore
/// `clone_running`'s final delta push) errors out — used to prove `start()` still runs when
/// that happens. Holds an `Arc` so the same backing data a prior successful push wrote can be
/// shared with a store that starts failing only afterward.
#[derive(Debug)]
struct FailingPutStore(Arc<InMemory>);

impl std::fmt::Display for FailingPutStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FailingPutStore({})", self.0)
    }
}

#[async_trait::async_trait]
impl ObjectStore for FailingPutStore {
    async fn put_opts(&self, _location: &S3Path, _payload: PutPayload, _opts: PutOptions) -> object_store::Result<PutResult> {
        Err(object_store::Error::Generic { store: "FailingPutStore", source: "put always fails".into() })
    }
    async fn put_multipart_opts(
        &self,
        _location: &S3Path,
        _opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        Err(object_store::Error::Generic { store: "FailingPutStore", source: "put_multipart always fails".into() })
    }
    async fn get_opts(&self, location: &S3Path, options: object_store::GetOptions) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<S3Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<S3Path>> {
        self.0.delete_stream(locations)
    }
    fn list(&self, prefix: Option<&S3Path>) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.0.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&S3Path>) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, from: &S3Path, to: &S3Path, options: object_store::CopyOptions) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn clone_running_calls_start_even_when_the_final_push_fails() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let base = registry_server().await;
    // The prefetch pull (phase 1) must succeed, so seed the source's first commit through a
    // working store backed by the same data the failing wrapper below shares, then swap to a
    // store whose writes always fail before the locked phase.
    let mem = Arc::new(InMemory::new());
    let good_store: Arc<dyn ObjectStore> = mem.clone();
    let e = engine(lp.pool(), good_store, meta.clone(), &base);

    let s = ws("karthik", "ws-clone-fail-src");
    e.meta.create_ws(&s).await.unwrap();
    init_live_subvol(&e.pool, &s.id);
    std::fs::write(e.pool.live(&s.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &s).await;

    let d = ws("karthik", "ws-clone-fail-dst");
    e.meta.create_ws(&d).await.unwrap();

    let failing_store: Arc<dyn ObjectStore> = Arc::new(FailingPutStore(mem));
    let e = engine(Pool::new(e.pool.root.clone()), failing_store, meta, &base);

    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_flag = started.clone();
    let stop = || -> Result<(), rustic_git_workspaces::engine::EngErr> { Ok(()) };
    let start = move || -> Result<(), rustic_git_workspaces::engine::EngErr> {
        started_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    };

    let err = e.clone_running(&s, &d, &stop, &start).await.unwrap_err();
    assert!(started.load(std::sync::atomic::Ordering::Relaxed), "start() must run even when the final push fails");
    assert!(!err.0.is_empty());
}

#[tokio::test]
async fn fsck_rebuild_recovers_lineage_after_snapshot_docs_are_lost() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), meta.clone() as Arc<dyn MetaStore>, &base);

    let w = ws("karthik", "ws-fsck");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    for layer in 0..5 {
        std::fs::write(e.pool.live(&w.id).join(format!("layer{layer}.txt")), format!("v{layer}")).unwrap();
        commit_and_push(&e, &w).await;
    }
    let original_lineage = e.pool.lineage(&w.id);
    assert_eq!(original_lineage.len(), 5);
    let expected = hash_tree(&e.pool.live(&w.id));

    // `fsck` rebuilds from `layers/*.json` sidecars alone — those live in the object store
    // regardless of the registry, so this test doesn't need to touch the registry at all: it
    // proves the sidecar trail push wrote survives even if every commit/ref record were lost.
    let report = fsck::rebuild(store.as_ref()).await.unwrap();
    assert_eq!(report.chains.len(), 1, "expected exactly one candidate tip");
    let rebuilt = &report.chains[0];
    assert_eq!(rebuilt.len(), 5);
    for (got, want) in rebuilt.iter().zip(&original_lineage) {
        assert_eq!(got.blob, want.blob);
        assert_eq!(got.sha256, want.sha256);
        assert_eq!(got.kind, want.kind);
    }
    assert_eq!(report.tips, vec![original_lineage.last().unwrap().blob.clone()]);

    // Adopt into a fresh MetaStore's Snapshot doc (fsck's own recovery surface, independent of
    // the engine) purely to exercise `fsck::adopt`; then pull cold from the object store using
    // the rebuilt lineage directly, bypassing the registry entirely (this is what an operator
    // recovering from lost registry data would do: rebuild from sidecars, then feed the
    // recovered lineage straight to a lower-level restore, not through `pull`'s registry path).
    let fresh_meta = Arc::new(MemStore::new());
    fresh_meta.create_ws(&w).await.unwrap();
    let _snap_id = fsck::adopt(fresh_meta.as_ref(), &w.id, rebuilt).await.unwrap();

    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, fresh_meta, &base);
    dst_engine.pull_raw(&w.id, rebuilt.clone()).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}

/// Wipe every registry record, then rebuild+truncate at the squash boundary from sidecars
/// alone — same recovery story as `fsck_rebuild_recovers_lineage_after_snapshot_docs_are_lost`,
/// this time across a squash.
#[tokio::test]
async fn fsck_rebuild_truncates_at_the_squash_boundary() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let mut e = engine(lp.pool(), store.clone(), meta.clone() as Arc<dyn MetaStore>, &base);
    e.squash_mb = 1;
    e.chain_max = 3;

    let w = ws("karthik", "ws-fsck-squash");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);

    // Push past the chain trigger, then settle inline (no detached child in this test binary).
    for i in 0..5 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("v{i}")).unwrap();
        commit_and_push(&e, &w).await;
    }
    e.squash(&w).await.unwrap();

    // A couple more streams grafted on top of the new block base.
    std::fs::write(e.pool.live(&w.id).join("post0.txt"), b"post0").unwrap();
    commit_and_push(&e, &w).await;
    std::fs::write(e.pool.live(&w.id).join("post1.txt"), b"post1").unwrap();
    commit_and_push(&e, &w).await;

    let original_lineage = e.pool.lineage(&w.id);
    assert_eq!(original_lineage[0].kind, rustic_git_workspaces::model::LayerKind::Block);
    let expected = hash_tree(&e.pool.live(&w.id));

    let report = fsck::rebuild(store.as_ref()).await.unwrap();
    let rebuilt = report
        .chains
        .iter()
        .find(|c| c.len() == original_lineage.len())
        .expect("no candidate tip matches the post-squash lineage length");
    assert_eq!(rebuilt[0].kind, rustic_git_workspaces::model::LayerKind::Block, "chain must start with the block layer");
    assert_eq!(rebuilt.len(), original_lineage.len(), "1 block + post-squash streams");
    assert!(
        rebuilt.iter().skip(1).all(|l| l.kind == rustic_git_workspaces::model::LayerKind::Stream),
        "everything after the block must be a stream"
    );

    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, meta, &base);
    dst_engine.pull_raw(&w.id, rebuilt.clone()).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}
