//! Engine op tests: push/pull/fork/clone/squash. Everything here touches btrfs, so every test
//! opens with `have_btrfs()` and returns cleanly when it's false (this Mac, any non-root CI
//! runner) — they run for real on the btrfs review VM. Fixtures: `MemStore` for
//! records/refs, `InMemory` object store for blobs, and a loopback btrfs pool per side
//! (mirrors `tests/engine_pool.rs`'s `LoopbackPool`).

use object_store::{ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult};
use object_store::memory::InMemory;
use object_store::path::Path as S3Path;
use rustic_git_workspaces::engine::{Engine, Pool, fsck, have_btrfs};
use rustic_git_workspaces::model::{Workspace, WsState};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use sha2::Digest;
use std::path::Path;
use std::sync::Arc;

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
        std::fs::create_dir_all(pool.root.join("ws")).unwrap();
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
        placement: None,
        ref_: None,
        quota_gb: 20,
        live_state: serde_json::Value::Null,
    }
}

fn engine(pool: Pool, store: Arc<dyn ObjectStore>, meta: Arc<dyn MetaStore>) -> Engine {
    Engine::new(pool, store, meta)
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
    std::fs::create_dir_all(pool.wsdir(ws_id)).unwrap();
    run(&["btrfs", "subvolume", "create", pool.live(ws_id).to_str().unwrap()]);
}

#[tokio::test]
async fn push_200_files_is_fast_and_sha_carrying() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(lp.pool(), store, meta.clone());

    let w = ws("karthik", "ws-push");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    for i in 0..200 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }

    let t = std::time::Instant::now();
    let out = e.push(&w).await.unwrap();
    assert!(t.elapsed().as_secs() < 1, "push of 200 small files took {:?}", t.elapsed());
    assert!(!out.sha.is_empty());
    assert_eq!(out.layers, 1);

    let (got, _) = meta.get_ws("karthik", "ws-push").await.unwrap().unwrap();
    assert!(got.ref_.is_some());
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
    let src_engine = engine(src.pool(), store.clone(), meta.clone());

    let w = ws("karthik", "ws-cold");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);
    for layer in 0..7 {
        std::fs::write(src_engine.pool.live(&w.id).join(format!("layer{layer}.txt")), format!("v{layer}")).unwrap();
        src_engine.push(&w).await.unwrap();
    }
    let expected = hash_tree(&src_engine.pool.live(&w.id));

    let dst_engine = engine(dst.pool(), store, meta);
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
    let e = engine(lp.pool(), store, meta);

    let w = ws("karthik", "ws-noop");
    e.meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    e.push(&w).await.unwrap();

    let out = e.pull(&w).await.unwrap();
    assert_eq!(out.fetched, 0);
}

#[tokio::test]
async fn fork_is_zero_fetch_and_isolated() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(lp.pool(), store, meta);

    let src = ws("karthik", "ws-fork-src");
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    e.push(&src).await.unwrap();

    let dst = ws("karthik", "ws-fork-dst");
    e.meta.create_ws(&dst).await.unwrap();
    e.fork(&src, &dst).await.unwrap();
    assert_eq!(hash_tree(&e.pool.live(&dst.id)), hash_tree(&e.pool.live(&src.id)));

    // A push after fork on either side must not affect the other (isolation).
    std::fs::write(e.pool.live(&dst.id).join("only-dst.txt"), b"dst").unwrap();
    e.push(&dst).await.unwrap();
    assert!(!e.pool.live(&src.id).join("only-dst.txt").exists());
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
    let mut e = engine(lp.pool(), store.clone(), meta.clone());
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
        let out = e.push(&w).await.unwrap();
        if let Some(r) = out.squash_triggered {
            assert!(r.contains("chain") || r.contains("MB"), "unexpected trigger reason: {r}");
            triggered = true;
            break;
        }
    }
    assert!(triggered, "chain trigger never fired within 5 pushes past chain_max=3");

    let latch = e.pool.root.join("ws").join(format!("{}.squashing", w.id));
    assert!(latch.exists(), "latch file must exist once a squash is triggered");

    // A second push while the latch is held must not spawn a second squash (message says so).
    std::fs::write(e.pool.live(&w.id).join("late.txt"), b"late").unwrap();
    let out2 = e.push(&w).await.unwrap();
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

    // Cold pull from the settled lineage must reproduce the same tree.
    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, meta);
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
    let src_engine = engine(src.pool(), store.clone(), meta.clone());

    let w = ws("karthik", "ws-corrupt");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);
    std::fs::write(src_engine.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    let out = src_engine.push(&w).await.unwrap();

    // Flip a byte in the uploaded blob directly in the InMemory store.
    let key = object_store::path::Path::from(format!("layers/{}.zst", out.layer));
    let mut bytes = store.get(&key).await.unwrap().bytes().await.unwrap().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    store.put(&key, bytes.into()).await.unwrap();

    let dst_engine = engine(dst.pool(), store, meta);
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
    let e = engine(lp.pool(), store, meta);

    let s = ws("karthik", "ws-clone-src");
    e.meta.create_ws(&s).await.unwrap();
    init_live_subvol(&e.pool, &s.id);
    std::fs::write(e.pool.live(&s.id).join("base.txt"), b"base").unwrap();
    e.push(&s).await.unwrap();

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
    let e = engine(lp.pool(), store, meta);

    let mut w = ws("karthik", "ws-state");
    w.live_state = serde_json::json!({"ports": [3000], "packages": ["node@22"]});
    e.meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    e.push(&w).await.unwrap();

    let (got_ws, _) = e.meta.get_ws("karthik", "ws-state").await.unwrap().unwrap();
    let r = got_ws.ref_.unwrap();
    let snap = e.meta.get_snapshot(&w.id, &r).await.unwrap().unwrap();
    assert_eq!(snap.state, w.live_state);
}

#[tokio::test]
async fn fork_inherits_snapshot_state_not_live_source_state() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(lp.pool(), store, meta);

    let mut src = ws("karthik", "ws-fork-state-src");
    src.live_state = serde_json::json!({"ports": [3000]});
    e.meta.create_ws(&src).await.unwrap();
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    e.push(&src).await.unwrap();

    // The source's live doc moves on after the push that fork will read; fork must inherit the
    // snapshot's captured state, not whatever the live doc says now.
    let (mut src_doc, etag) = e.meta.get_ws("karthik", "ws-fork-state-src").await.unwrap().unwrap();
    src_doc.live_state = serde_json::json!({"ports": [9999]});
    e.meta.replace_ws(&src_doc, &etag).await.unwrap();

    let dst = ws("karthik", "ws-fork-state-dst");
    e.meta.create_ws(&dst).await.unwrap();
    e.fork(&src, &dst).await.unwrap();

    let (dst_doc, _) = e.meta.get_ws("karthik", "ws-fork-state-dst").await.unwrap().unwrap();
    assert_eq!(dst_doc.live_state, serde_json::json!({"ports": [3000]}));
}

#[tokio::test]
async fn create_from_snapshot_restores_an_older_record_not_the_tip() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let src_pool = LoopbackPool::new();
    let dst_pool = LoopbackPool::new();
    let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let src_engine = engine(src_pool.pool(), store.clone(), meta.clone());

    let mut w = ws("karthik", "ws-older");
    w.live_state = serde_json::json!({"packages": ["node@20"]});
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&src_engine.pool, &w.id);

    std::fs::write(src_engine.pool.live(&w.id).join("f.txt"), b"v1").unwrap();
    src_engine.push(&w).await.unwrap();
    let (older_doc, _) = meta.get_ws("karthik", "ws-older").await.unwrap().unwrap();
    let older_snapshot_id = older_doc.ref_.clone().unwrap();

    // Advance past the older snapshot: change the file's content and the live state, then push
    // again so the ref tip no longer matches what we're about to restore.
    std::fs::write(src_engine.pool.live(&w.id).join("f.txt"), b"v2-changed-after").unwrap();
    let (mut latest, etag) = meta.get_ws("karthik", "ws-older").await.unwrap().unwrap();
    latest.live_state = serde_json::json!({"packages": ["node@22"]});
    meta.replace_ws(&latest, &etag).await.unwrap();
    src_engine.push(&w).await.unwrap();

    let dst = ws("karthik", "ws-from-older-snapshot");
    meta.create_ws(&dst).await.unwrap();
    let dst_engine = engine(dst_pool.pool(), store, meta.clone());
    dst_engine.create_from_snapshot(&w.id, &older_snapshot_id, &dst).await.unwrap();

    assert_eq!(std::fs::read(dst_engine.pool.live(&dst.id).join("f.txt")).unwrap(), b"v1");
    let (dst_doc, _) = meta.get_ws("karthik", "ws-from-older-snapshot").await.unwrap().unwrap();
    assert_eq!(dst_doc.live_state, serde_json::json!({"packages": ["node@20"]}));
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
    // The prefetch pull (phase 1) must succeed, so seed the source's first snapshot through a
    // working store backed by the same data the failing wrapper below shares, then swap to a
    // store whose writes always fail before the locked phase.
    let mem = Arc::new(InMemory::new());
    let good_store: Arc<dyn ObjectStore> = mem.clone();
    let e = engine(lp.pool(), good_store, meta.clone());

    let s = ws("karthik", "ws-clone-fail-src");
    e.meta.create_ws(&s).await.unwrap();
    init_live_subvol(&e.pool, &s.id);
    std::fs::write(e.pool.live(&s.id).join("base.txt"), b"base").unwrap();
    e.push(&s).await.unwrap();

    let d = ws("karthik", "ws-clone-fail-dst");
    e.meta.create_ws(&d).await.unwrap();

    let failing_store: Arc<dyn ObjectStore> = Arc::new(FailingPutStore(mem));
    let e = engine(Pool::new(e.pool.root.clone()), failing_store, meta);

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
    let e = engine(lp.pool(), store.clone(), meta.clone() as Arc<dyn MetaStore>);

    let w = ws("karthik", "ws-fsck");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);
    for layer in 0..5 {
        std::fs::write(e.pool.live(&w.id).join(format!("layer{layer}.txt")), format!("v{layer}")).unwrap();
        e.push(&w).await.unwrap();
    }
    let original_lineage = e.pool.lineage(&w.id);
    assert_eq!(original_lineage.len(), 5);
    let expected = hash_tree(&e.pool.live(&w.id));

    // Wipe every Snapshot doc, simulating metadata loss the sidecars must survive.
    let meta = Arc::new(MemStore::new());
    meta.create_ws(&w).await.unwrap();

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

    let snap_id = fsck::adopt(meta.as_ref(), &w.id, rebuilt).await.unwrap();
    let (mut w_doc, etag) = meta.get_ws(&w.owner, &w.id).await.unwrap().unwrap();
    w_doc.ref_ = Some(snap_id);
    meta.replace_ws(&w_doc, &etag).await.unwrap();
    let w = w_doc;

    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, meta);
    dst_engine.pull(&w).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}

#[tokio::test]
async fn fsck_rebuild_truncates_at_the_squash_boundary() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let meta = Arc::new(MemStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut e = engine(lp.pool(), store.clone(), meta.clone() as Arc<dyn MetaStore>);
    e.squash_mb = 1;
    e.chain_max = 3;

    let w = ws("karthik", "ws-fsck-squash");
    meta.create_ws(&w).await.unwrap();
    init_live_subvol(&e.pool, &w.id);

    // Push past the chain trigger, then settle inline (no detached child in this test binary).
    for i in 0..5 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("v{i}")).unwrap();
        e.push(&w).await.unwrap();
    }
    e.squash(&w).await.unwrap();

    // A couple more streams grafted on top of the new block base.
    std::fs::write(e.pool.live(&w.id).join("post0.txt"), b"post0").unwrap();
    e.push(&w).await.unwrap();
    std::fs::write(e.pool.live(&w.id).join("post1.txt"), b"post1").unwrap();
    e.push(&w).await.unwrap();

    let original_lineage = e.pool.lineage(&w.id);
    assert_eq!(original_lineage[0].kind, rustic_git_workspaces::model::LayerKind::Block);
    let expected = hash_tree(&e.pool.live(&w.id));

    // Wipe Snapshot docs; rebuild from sidecars alone.
    let meta = Arc::new(MemStore::new());
    meta.create_ws(&w).await.unwrap();

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

    let snap_id = fsck::adopt(meta.as_ref(), &w.id, rebuilt).await.unwrap();
    let (mut w_doc, etag) = meta.get_ws(&w.owner, &w.id).await.unwrap().unwrap();
    w_doc.ref_ = Some(snap_id);
    meta.replace_ws(&w_doc, &etag).await.unwrap();
    let w = w_doc;

    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, meta);
    dst_engine.pull(&w).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}
