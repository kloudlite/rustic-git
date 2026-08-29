//! Engine op tests: commit/push/clone_local_ids/restore/squash. Everything here touches btrfs, so every
//! test opens with `have_btrfs()` and returns cleanly when it's false (this Mac, any non-root CI
//! runner) — they run for real on the btrfs review VM. Fixtures: an in-process vol-agent router (`registry_server`, mirroring
//! `bins/agent/tests/loop.rs`) as the volume registry, and an `InMemory` object store for layer
//! blobs.

use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt};
use object_store::memory::InMemory;
use object_store::path::Path as S3Path;
use rustic_git_workspaces::engine::{Engine, Pool, have_btrfs};
use rustic_git_workspaces::model::{Workspace, WsState};
use rustic_git_workspaces::registry::CommitRecord;
use rustic_git_workspaces::registry_client::RegistryClient;
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
    let ownership = rustic_git_server::ownership::OwnershipStore::open(os_store.os.clone());
    let app = rustic_git_server::App::new(
        os_store,
        Arc::new(ownership),
        "test-0".into(),
        Arc::new(|_| "127.0.0.1:1".to_string()),
        "test-peer-secret".into(),
        rustic_git_server::pulls::Source::Absent,
    );
    app.election_tick().await.unwrap();
    let app = Arc::new(app);
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
        team: String::new(),
        id: id.into(),
        owner: owner.into(),
        name: id.into(),
        region: "centralindia".into(),
        state: WsState::Ready,
        image: "nginx:alpine".into(),
        placement: None,
        volume: None,
        quota_gb: 20,
        ssh: None,
        live_state: serde_json::Value::Null,
        packages: vec![],
        base_packages: vec![],
        packages_status: None,
    }
}

fn engine(pool: Pool, store: Arc<dyn ObjectStore>, registry_base: &str) -> Engine {
    Engine::new(pool, store, RegistryClient::new(registry_base, TOKEN))
}

/// `Engine::push_env` with no message — the common case for tests whose point isn't the message
/// itself.
async fn commit_and_push(e: &Engine, w: &Workspace) -> rustic_git_workspaces::engine::PushOut {
    e.push_env(&w.owner, &w.id, &w.live_state, None).await.unwrap()
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), &base);

    let w = ws("karthik", "ws-push-msg");
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();

    let out = e.push_env(&w.owner, &w.id, &w.live_state, Some("first push")).await.unwrap();
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), &base);

    let w = ws("karthik", "ws-push");
    init_live_subvol(&e.pool, &w.id);
    for i in 0..200 {
        std::fs::write(e.pool.live(&w.id).join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }

    let t = std::time::Instant::now();
    let out = e.push_env(&w.owner, &w.id, &w.live_state, None).await.unwrap();
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;

    let w = ws("karthik", "ws-push-retry");
    // Same pool underneath both engines below — only the registry endpoint differs.
    let pool_root = lp.pool.root.clone();
    init_live_subvol(&lp.pool, &w.id);
    std::fs::write(lp.pool.live(&w.id).join("a.txt"), b"a").unwrap();

    // Port 1: nothing listens there, so any request errors out immediately — same stand-in
    // address `registry_server`'s own `App::new` fixture uses for "unreachable peer". A push
    // against it still stages (snapshot + compress, local-only) before the registry call it
    // never reaches — the crash-recovery window `push` is meant to survive.
    let broken = engine(Pool::new(pool_root.clone()), store.clone(), "http://127.0.0.1:1");
    let err = broken.push_env(&w.owner, &w.id, &w.live_state, None).await.unwrap_err();
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
    let good = engine(Pool::new(pool_root), store, &base);
    let out = good.push_env(&w.owner, &w.id, &w.live_state, None).await.unwrap();
    assert_eq!(out.layers, 2, "the retried push's own snapshot plus the one stranded by the failed attempt");
    let recs = history(&base, &w.owner, &w.id).await;
    assert_eq!(recs.len(), 2, "the retry must land the stranded record, not lose or duplicate it");
    assert!(good.pool.lineage(&w.id).iter().all(|l| !l.unpushed));
    assert!(!good.pool.stage_path(&staged_blob).exists(), "a successful push must clean up its stage files");
}

/// `commit_core` snapshots BEFORE the send. A send that fails used to leave that RO snapshot in
/// `recv/` with no lineage entry naming it — invisible to every reclaim path, pinning extents for
/// good. A parent that does not exist is the cheapest way to make `btrfs send -p` fail.
#[tokio::test]
async fn a_failed_send_leaves_no_stray_snapshot_behind() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let base = registry_server().await;
    let e = engine(lp.pool(), Arc::new(InMemory::new()), &base);
    let w = ws("karthik", "ws-send-fails");
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    let bogus = rustic_git_workspaces::model::LineageEntry {
        kind: rustic_git_workspaces::model::LayerKind::Stream,
        blob: "never-received".into(),
        snap: None,
        sha256: "sha".into(),
        unpushed: false,
    };
    e.pool.set_lineage(&w.id, std::slice::from_ref(&bogus)).unwrap();
    let before: Vec<_> = std::fs::read_dir(e.pool.recv()).unwrap().flatten().map(|d| d.file_name()).collect();

    let err = e.push_env(&w.owner, &w.id, &w.live_state, None).await.unwrap_err();
    assert!(err.0.contains("btrfs send"), "unexpected error: {}", err.0);

    let after: Vec<_> = std::fs::read_dir(e.pool.recv()).unwrap().flatten().map(|d| d.file_name()).collect();
    assert_eq!(before, after, "the pre-send snapshot must be deleted with the failed send");
    assert_eq!(e.pool.lineage(&w.id).len(), 1, "no entry for a layer that was never staged");
}

/// `spec.quotaGb` is a qgroup limit on the live subvolume. Before the pool has quotas enabled the
/// engine says so instead of failing (the operator's fix is one command); after, a tenant writing
/// past the cap gets EDQUOT while the pool — and every sibling on it — stays writable.
#[tokio::test]
async fn quota_is_reported_unavailable_then_enforced_once_the_pool_has_qgroups() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let base = registry_server().await;
    let e = engine(lp.pool(), Arc::new(InMemory::new()), &base);
    e.create_subvol("ws-quota").unwrap();
    assert!(e.set_quota("ws-quota", 1).unwrap().is_some(), "a pool without qgroups must say so, not fail");

    run(&["btrfs", "quota", "enable", lp.pool.root.to_str().unwrap()]);
    run(&["btrfs", "quota", "rescan", "-w", lp.pool.root.to_str().unwrap()]);
    assert_eq!(e.set_quota("ws-quota", 1).unwrap(), None);

    // 1 GiB cap on a 4 GiB pool: the writes must stop well before the pool does.
    let chunk = vec![0xabu8; 64 << 20];
    let mut written = 0u64;
    let mut hit = false;
    for i in 0..48 {
        let p = e.pool.live("ws-quota").join(format!("fill-{i}"));
        match std::fs::write(&p, &chunk) {
            Ok(()) => written += chunk.len() as u64,
            Err(_) => {
                hit = true;
                break;
            }
        }
    }
    assert!(hit, "wrote {written} bytes past a 1 GiB quota without an error");
    assert!(written < 2 << 30, "the cap must bite near the limit, not the pool: {written}");
    e.create_subvol("ws-sibling").unwrap();
    std::fs::write(e.pool.live("ws-sibling").join("still-writable"), b"x").expect("a sibling is unaffected");
}

#[tokio::test]
async fn seven_layer_cold_pull_is_byte_identical() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let src = LoopbackPool::new();
    let dst = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src.pool(), store.clone(), &base);

    let w = ws("karthik", "ws-cold");
    init_live_subvol(&src_engine.pool, &w.id);
    for layer in 0..7 {
        std::fs::write(src_engine.pool.live(&w.id).join(format!("layer{layer}.txt")), format!("v{layer}")).unwrap();
        commit_and_push(&src_engine, &w).await;
    }
    let expected = hash_tree(&src_engine.pool.live(&w.id));

    let dst_engine = engine(dst.pool(), store, &base);
    dst_engine.clone_local_ids(&w.owner, &w.id, &w.id).await.unwrap();
    assert_eq!(hash_tree(&dst_engine.pool.live(&w.id)), expected);
}

#[tokio::test]
async fn clone_is_zero_fetch_and_isolated() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let src = ws("karthik", "ws-clone-src");
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &src).await;

    let dst = ws("karthik", "ws-clone-dst");
    e.clone_local_ids(&src.owner, &src.id, &dst.id).await.unwrap();
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
/// snapshot, just a live subvolume with a write in it — yet `clone_local_ids` still succeeds: no
/// registry call, dst tree byte-identical to src's live subvolume, and dst starts equally
/// lineage-less. Then dst's own push works from that lineage-less state.
#[tokio::test]
async fn clone_of_never_pushed_workspace_is_local() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let src = ws("karthik", "ws-clone-nopush-src");
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    assert!(e.pool.lineage(&src.id).is_empty(), "src has no snapshot at all yet");

    let dst = ws("karthik", "ws-clone-nopush-dst");
    e.clone_local_ids(&src.owner, &src.id, &dst.id).await.unwrap(); // must succeed locally, no push required first

    assert_eq!(hash_tree(&e.pool.live(&dst.id)), hash_tree(&e.pool.live(&src.id)));
    assert!(e.pool.lineage(&dst.id).is_empty(), "dst inherits src's lineage-less state verbatim");

    // No registry call: dst has no history until its own push.
    assert!(history(&base, &dst.owner, &dst.id).await.is_empty());

    // dst's own push still works from here.
    e.push_env(&dst.owner, &dst.id, &dst.live_state, None).await.unwrap();
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let src = ws("karthik", "ws-clone2-src");
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();

    let dst = ws("karthik", "ws-clone2-dst");
    e.clone_local_ids(&src.owner, &src.id, &dst.id).await.unwrap();

    let dst2 = ws("karthik", "ws-clone2-dst2");
    e.clone_local_ids(&dst.owner, &dst.id, &dst2.id).await.unwrap();

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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let mut e = engine(lp.pool(), store.clone(), &base);
    e.squash_mb = 1; // 1MB trigger, not the 256MB default, for test speed
    e.chain_max = 3; // chain trigger, not the default 50

    let w = ws("karthik", "ws-squash");
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
    e.squash(&w.owner, &w.id, serde_json::Value::Null).await.unwrap();
    assert!(!latch.exists(), "squash must remove its own latch when done");
    // The build image is disposable the moment its bytes are uploaded — keeping it grew the pool
    // by one full workspace image per squash, and the janitor cannot reclaim it by lineage
    // (the new lineage references it).
    assert_eq!(
        std::fs::read_dir(e.pool.img_dir()).map(|d| d.count()).unwrap_or(0),
        0,
        "squash must delete its build image after upload"
    );
    assert!(
        std::fs::read_dir("/tmp").into_iter().flatten().flatten().all(|d| !d.file_name().to_string_lossy().starts_with("wssquash-")),
        "squash must leave no throwaway mount directory behind"
    );

    let lineage = e.pool.lineage(&w.id);
    assert_eq!(lineage[0].kind, rustic_git_workspaces::model::LayerKind::Block);
    assert!(
        lineage.iter().skip(1).all(|l| l.kind == rustic_git_workspaces::model::LayerKind::Stream),
        "post-tip pushes must graft as streams onto the new block base"
    );
    assert!(lineage.iter().all(|l| !l.unpushed), "squash's own push must clear every mark");

    // Cold pull from the settled lineage must reproduce the same tree.
    let dst = LoopbackPool::new();
    let dst_engine = engine(dst.pool(), store, &base);
    dst_engine.clone_local_ids(&w.owner, &w.id, &w.id).await.unwrap();
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src.pool(), store.clone(), &base);

    let w = ws("karthik", "ws-corrupt");
    init_live_subvol(&src_engine.pool, &w.id);
    std::fs::write(src_engine.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    let out = commit_and_push(&src_engine, &w).await;

    // Flip a byte in the uploaded blob directly in the InMemory store.
    let key = object_store::path::Path::from(format!("layers/{}.zst", out.layer));
    let mut bytes = store.get(&key).await.unwrap().bytes().await.unwrap().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    store.put(&key, bytes.into()).await.unwrap();

    let dst_engine = engine(dst.pool(), store, &base);
    let err = dst_engine.clone_local_ids(&w.owner, &w.id, &w.id).await.unwrap_err();
    assert!(err.0.contains("sha mismatch"), "unexpected error: {}", err.0);
}

#[tokio::test]
async fn push_captures_live_state_into_the_record() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let mut w = ws("karthik", "ws-state");
    w.live_state = serde_json::json!({"ports": [3000], "packages": ["node@22"]});
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let mut src = ws("karthik", "ws-clone-state-src");
    src.live_state = serde_json::json!({"ports": [3000]});
    init_live_subvol(&e.pool, &src.id);
    std::fs::write(e.pool.live(&src.id).join("base.txt"), b"base").unwrap();
    commit_and_push(&e, &src).await;

    // The source's doc drifts after that push — a later push would capture THIS value, not the
    // one already durable in history.
    src.live_state = serde_json::json!({"ports": [9999]});

    // `clone_local_ids` only ever copies FILES (the local-first lineage/subvolume); it never reads
    // or writes `live_state` — that's `crates/workspaces/src/api.rs`'s `clone_ws` handler's job,
    // which builds the destination doc with `live_state: src.live_state.clone()` (the source's
    // CURRENT value at clone time, same "current state, not an old snapshot" rule clone already
    // applies to file content — `restore` is the verb for an explicit past snapshot). Mirror
    // that here since this test drives the engine directly, under the API.
    let mut dst = ws("karthik", "ws-clone-state-dst");
    dst.live_state = src.live_state.clone();
    e.clone_local_ids(&src.owner, &src.id, &dst.id).await.unwrap();

    // `push` always captures the live doc's OWN `live_state` at push time (no more re-deriving
    // it from an inherited lineage entry) — so dst's first push registers whatever `dst`'s own
    // doc says, which the API set to src's state as of the clone request.
    e.push_env(&dst.owner, &dst.id, &dst.live_state, None).await.unwrap();
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let src_engine = engine(src_pool.pool(), store.clone(), &base);

    let mut w = ws("karthik", "ws-older");
    w.live_state = serde_json::json!({"packages": ["node@20"]});
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
    let dst_engine = engine(dst_pool.pool(), store, &base);
    dst_engine.restore(&w.owner, &w.id, &older_commit_id, &dst.id, None).await.unwrap();

    assert_eq!(std::fs::read(dst_engine.pool.live(&dst.id).join("f.txt")).unwrap(), b"v1");

    // `push` always captures the live doc's OWN `live_state` at push time — dst's first push
    // registers whatever `dst`'s doc says, which the API set to the restored snapshot's state.
    // `restore` itself already staged the restored entry as unpushed (ready to register under
    // dst's own history); the fused `push` takes ONE MORE fresh snapshot on top of that before
    // uploading (see `ops.rs::push`'s doc — every push always snapshots, restore/clone-then-push
    // included, even when nothing changed since materializing), so dst ends up with both: the
    // restored entry plus dst's own redundant-but-harmless new one.
    dst_engine.push_env(&dst.owner, &dst.id, &dst.live_state, None).await.unwrap();
    let dst_recs = history(&base, &dst.owner, &dst.id).await;
    assert_eq!(dst_recs.len(), 2, "the restored entry plus push's own fresh snapshot on top of it");
    assert_eq!(dst_recs[0].state, serde_json::json!({"packages": ["node@20"]}));
}

/// A controller restart re-runs reconcile from scratch, so create/clone against a subvolume that
/// already exists must be a no-op, not an error that marks a healthy workspace Error. This is the
/// half of audit H2 that survives deleting the lease: without the lease there is no "one attempt at
/// a time" guarantee to lean on, only convergence.
#[tokio::test]
async fn create_and_clone_are_idempotent_against_an_existing_live_subvolume() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store.clone(), &base);

    let src = ws("karthik", "ws-idem-src");

    // create_subvol, twice. The marker proves the second call kept the FIRST subvolume rather
    // than quietly replacing it — converging by deleting would be data loss dressed as success.
    e.create_subvol(&src.id).unwrap();
    std::fs::write(e.pool.live(&src.id).join("keep.txt"), b"keep").unwrap();
    e.create_subvol(&src.id).expect("a replayed create must converge, not fail");
    assert_eq!(
        std::fs::read(e.pool.live(&src.id).join("keep.txt")).unwrap(),
        b"keep",
        "a replayed create must leave the existing subvolume's contents alone"
    );

    // clone_local_ids, twice, against a source that has never pushed.
    let dst = ws("karthik", "ws-idem-dst");
    e.clone_local_ids(&src.owner, &src.id, &dst.id).await.unwrap();
    std::fs::write(e.pool.live(&dst.id).join("dst-marker.txt"), b"dst").unwrap();
    e.clone_local_ids(&src.owner, &src.id, &dst.id)
        .await
        .expect("a replayed clone must converge, not fail");
    assert_eq!(
        std::fs::read(e.pool.live(&dst.id).join("dst-marker.txt")).unwrap(),
        b"dst",
        "a replayed clone must not recreate a destination that already exists"
    );
    assert_eq!(
        std::fs::read(e.pool.live(&dst.id).join("keep.txt")).unwrap(),
        b"keep",
        "and the clone must still carry the source's content"
    );
    // The failure mode this actually guards against, verified against btrfs-progs 6.6.3: neither
    // `subvolume create` nor `subvolume snapshot` FAILS on an existing target — both exit 0.
    // `create` merely prints an error, but `snapshot` silently nests a whole second subvolume at
    // `{dst}/{basename(src)}`, i.e. `live/live`. That corrupts the destination invisibly: a nested
    // subvolume cannot be `btrfs send`-ed, so the next push of this clone would fail, and no
    // cleanup path knows the nested one exists. An exit code cannot catch this — only the absence
    // of the nested path can.
    assert!(
        !e.pool.live(&dst.id).join("live").exists(),
        "a replayed clone must not nest a second subvolume inside the destination"
    );
}

/// The 27 Aug hang, as a test: a lineage whose blob is not in the store must come back as an
/// ERROR, promptly, and named. This one needs no btrfs — the failure is meant to happen before
/// anything touches a subvolume, which is also why the assertion can be "no `receive` ever ran".
#[tokio::test]
async fn a_missing_layer_blob_fails_fast_instead_of_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // Port 1: nothing listens. `pull_core` bypasses the registry entirely, so this is never called.
    let e = engine(Pool::new(tmp.path()), store, "http://127.0.0.1:1");
    let lineage = vec![rustic_git_workspaces::model::LineageEntry {
        kind: rustic_git_workspaces::model::LayerKind::Stream,
        blob: "no-such-blob".into(),
        snap: Some("no-such-snap".into()),
        sha256: "0".repeat(64),
        unpushed: false,
    }];

    let t0 = std::time::Instant::now();
    let err = e.pull_core("vol-x", lineage, &e.store).await.expect_err("a blob that is not there is an error");

    assert!(
        err.to_string().contains(rustic_git_workspaces::engine::ops::FETCH_FAILED),
        "the failure must be the one the agent classifies as permanent: {err}"
    );
    // Well inside `blob::GET_TIMEOUT`: an InMemory miss is answered, not waited out. The bound is
    // what makes an UNANSWERED store finite too.
    assert!(t0.elapsed() < std::time::Duration::from_secs(30), "took {:?}", t0.elapsed());
}

/// A restore naming a region this node has no credentials for is refused BEFORE the registry is
/// read — no store to read it from is a permanent fact about the deploy, not an outage.
#[tokio::test]
async fn a_restore_from_an_unknown_region_fails_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(Pool::new(tmp.path()), store, "http://127.0.0.1:1");

    let err = e
        .restore("alice", "env-1", "snap-1", "ws-2", Some("centralindia-vm"))
        .await
        .expect_err("no credentials for that region");
    let msg = err.to_string();
    assert!(msg.contains(rustic_git_workspaces::engine::ops::REGION_UNREACHABLE), "{msg}");
    assert!(msg.contains("centralindia-vm"), "the condition has to name the region: {msg}");

    // The engine's own region always resolves, so a same-region restore gets as far as the
    // registry (which is not listening here) rather than being refused for credentials.
    let same = e.restore("alice", "env-1", "snap-1", "ws-2", Some(&e.region)).await.expect_err("no registry");
    assert!(!same.to_string().contains(rustic_git_workspaces::engine::ops::REGION_UNREACHABLE), "{same}");
}

/// The in-place restore's swap half: `live` becomes the restored snapshot, the bytes it replaced
/// survive as a local RO snapshot, and the restored lineage becomes this volume's own (or its next
/// push would delta against a history the disk no longer holds).
#[tokio::test]
async fn replace_live_swaps_the_subvolume_and_keeps_the_old_one() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let w = ws("karthik", "ws-inplace");
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    commit_and_push(&e, &w).await;
    let snapshot_id = history(&base, &w.owner, &w.id).await[0].id.clone();

    // The change the restore is meant to discard.
    std::fs::write(e.pool.live(&w.id).join("b.txt"), b"b").unwrap();

    let staging = format!("{}-restoring", w.id);
    e.restore(&w.owner, &w.id, &snapshot_id, &staging, None).await.unwrap();
    e.replace_live(&w.id, &staging).unwrap();

    assert!(e.pool.live(&w.id).join("a.txt").exists());
    assert!(!e.pool.live(&w.id).join("b.txt").exists(), "the restore discards later changes");
    assert!(!e.pool.live(&staging).exists(), "the staging subvolume is not left behind");
    assert_eq!(e.pool.lineage(&w.id).len(), 1, "the restored lineage is the volume's own now");

    // Rollback is a plain btrfs snapshot off this, by hand — which is the whole reason it is kept.
    let safety: Vec<_> = std::fs::read_dir(e.pool.voldir(&w.id))
        .unwrap()
        .filter_map(|d| d.ok())
        .filter(|d| d.file_name().to_string_lossy().starts_with("before-restore-"))
        .collect();
    assert_eq!(safety.len(), 1, "exactly one safety snapshot");
    assert!(safety[0].path().join("b.txt").exists(), "the discarded state is still on disk");
}

/// Staging is torn down before it is built. `pull_core` treats an existing `live` as "already
/// converged", so bytes left behind by a restore that failed half-way would be swapped in by the
/// NEXT restore and labelled as ITS snapshot — the wrong data under the right name.
#[tokio::test]
async fn a_leftover_staging_subvolume_is_discarded_before_the_next_restore() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let base = registry_server().await;
    let e = engine(lp.pool(), store, &base);

    let w = ws("karthik", "ws-stale-staging");
    init_live_subvol(&e.pool, &w.id);
    std::fs::write(e.pool.live(&w.id).join("a.txt"), b"a").unwrap();
    commit_and_push(&e, &w).await;
    let snapshot_id = history(&base, &w.owner, &w.id).await[0].id.clone();

    // What a failed restore leaves: a materialized staging subvolume holding the WRONG bytes.
    let staging = format!("{}-restoring", w.id);
    init_live_subvol(&e.pool, &staging);
    std::fs::write(e.pool.live(&staging).join("stale.txt"), b"stale").unwrap();

    e.discard_staging(&staging).unwrap();
    assert!(!e.pool.live(&staging).exists(), "the stale staging subvolume is gone");

    e.restore(&w.owner, &w.id, &snapshot_id, &staging, None).await.unwrap();
    e.replace_live(&w.id, &staging).unwrap();
    assert!(e.pool.live(&w.id).join("a.txt").exists());
    assert!(!e.pool.live(&w.id).join("stale.txt").exists(), "the stale bytes must never reach live");
}

/// The whole point of nesting: `btrfs send` never descends into a nested subvolume and the parent's
/// qgroup does not count it, so a cache never uploads and never eats the home's quota — and a
/// restore, which receives a stream with no trace of them, has to make them again.
#[tokio::test]
async fn a_homes_cache_subvolumes_stay_out_of_the_push_and_come_back_after_a_restore() {
    use std::os::unix::fs::MetadataExt;
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let base = registry_server().await;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(lp.pool(), store.clone(), &base);
    e.create_subvol("home-alice").unwrap();
    e.ensure_home_dirs("home-alice", 1000).unwrap();
    let live = e.pool.live("home-alice");
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS {
        assert!(live.join(rel).is_dir(), "{rel}");
        assert_eq!(std::fs::metadata(live.join(rel)).unwrap().uid(), 1000, "{rel} must be the owner's, not root's");
    }
    assert_eq!(std::fs::metadata(live.join(".cargo")).unwrap().uid(), 1000, "the parent dir too, or `mkdir ~/.cargo/x` fails as kl");
    // A nested subvolume has its own inode 256, a plain directory does not.
    assert_eq!(std::fs::metadata(live.join(".cache")).unwrap().ino(), 256);
    std::fs::write(live.join(".zshrc"), b"alias ll='ls -l'").unwrap();
    std::fs::write(live.join(".cache").join("big"), vec![1u8; 1 << 20]).unwrap();

    e.sync_pool().unwrap();
    let out = e.push_env("alice", "home-alice", &serde_json::Value::Null, Some("home: periodic")).await.unwrap();
    // What the timer records is the SNAPSHOT's generation: with nothing written since, live is at
    // or behind it (the snapshot's own transaction may or may not have moved live), so nothing is
    // due — and one write later it is strictly past it. Reading live after the push instead would
    // fold a write that landed in between into the recorded number and never push it.
    let pushed = e.pushed_generation("home-alice", &out.layer).unwrap();
    e.sync_pool().unwrap();
    assert!(e.generation("home-alice").unwrap() <= pushed, "nothing changed since the push, nothing is due");
    // Idempotent: everything is present, nothing is recreated, nothing is lost.
    e.ensure_home_dirs("home-alice", 1000).unwrap();
    assert!(live.join(".cache").join("big").exists());
    std::fs::write(live.join("touched"), b"x").unwrap();
    e.sync_pool().unwrap();
    assert!(e.generation("home-alice").unwrap() > pushed, "a write moves the generation past the snapshot's");

    let tip = history(&base, "alice", "home-alice").await[0].id.clone();
    e.restore("alice", "home-alice", &tip, "home-alice-2", None).await.unwrap();
    let live2 = e.pool.live("home-alice-2");
    assert_eq!(std::fs::read(live2.join(".zshrc")).unwrap(), b"alias ll='ls -l'");
    assert!(!live2.join(".cache").join("big").exists(), "nested subvolumes are never in the send stream");
    e.ensure_home_dirs("home-alice-2", 1000).unwrap();
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS {
        assert!(live2.join(rel).is_dir(), "{rel} must be recreated after a restore");
    }

    // The pull: a node with no subvolume for this home gets the registry's `main` — the path a
    // person's first workspace on a new node takes. The nested subvolumes go first (btrfs will
    // not delete a parent over them), which is also why a received home has none until
    // `ensure_home_dirs` runs, as the Volume reconcile does right after.
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS.iter().rev() {
        run(&["btrfs", "subvolume", "delete", live.join(rel).to_str().unwrap()]);
    }
    run(&["btrfs", "subvolume", "delete", live.to_str().unwrap()]);
    assert!(!live.exists());
    e.materialize_home("alice", "home-alice").await.unwrap();
    assert_eq!(std::fs::read(live.join(".zshrc")).unwrap(), b"alias ll='ls -l'", "the pushed rc file is back");
    assert!(!live.join("touched").exists(), "written after the push, so not in the registry's copy");
    assert!(!live.join(".cache").join("big").exists());
    e.ensure_home_dirs("home-alice", 1000).unwrap();
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS {
        assert_eq!(std::fs::metadata(live.join(rel)).unwrap().ino(), 256, "{rel} must be a nested subvolume again");
        assert_eq!(std::fs::metadata(live.join(rel)).unwrap().uid(), 1000, "{rel}");
    }
}
