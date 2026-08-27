//! Process setup for `rustic-git-agent`: the local `Engine`, the storage janitor, and the
//! Kubernetes client the node controller (`controller.rs`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.

use rustic_git_workspaces::engine::{blob, Engine, Pool};
use rustic_git_workspaces::model::{LayerKind, LineageEntry};
use rustic_git_workspaces::store::MetaStore;
use std::sync::Arc;

pub mod claim;
pub mod controller;

/// Placement's node-picking algorithm still lives in `rustic_git_workspaces::placement` because
/// `bins/api` is still its caller until that path is deleted; re-exported here so the agent-side
/// name in the design is real rather than a second copy of the algorithm.
pub use rustic_git_workspaces::placement;

/// Env-derived config shared by `run` and the `squash` subcommand — both need the same Engine.
pub struct Config {
    pub api_url: String,
    pub region: String,
    pub agent_token: String,
    pub pool: String,
    pub hostname: String,
    /// This node's name, from the downward-API `NODE_NAME`. It is the shard key: the controller
    /// watches only objects whose `spec.nodeName` equals it.
    pub node: String,
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
                    tracing::warn!(
                        "rustic-git-agent: WS_API_URL is deprecated for the agent work surface, use WS_REGISTRY_URL (points at the server tier, not bins/api)"
                    );
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
            // Declared capacity is gone: the kubelet reports node allocatable, and a second
            // hand-maintained copy of it is a second thing that can be wrong.
            node: std::env::var("NODE_NAME").unwrap_or_default(),
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

/// Boots the node controller: Engine, janitor, Kubernetes client, then reconcile forever.
pub async fn run(cfg: Config) -> Result<(), String> {
    let meta = meta_store_from_env().await?;
    let engine = Arc::new(build_engine(&cfg.pool, meta, &cfg.api_url, &cfg.agent_token));
    spawn_janitor(engine.clone(), cfg.pool.clone());
    if cfg.node.is_empty() {
        return Err("NODE_NAME is unset: the controller would watch every node's objects".into());
    }
    // The CRDs must be Established before the watch starts, or it fails at startup and the
    // controller sits idle looking healthy. Fail loudly here rather than in production.
    let client = kube::Client::try_default().await.map_err(|e| e.to_string())?;
    let roles = node_roles(&client, &cfg.node).await;
    tracing::info!(node = %cfg.node, ?roles, "node roles");
    controller::run(Arc::new(controller::Ctx::new(client, engine, cfg.node, cfg.pool, cfg.region, roles))).await
}


/// The roles this node advertises. An unreadable Node object yields no roles, so the agent
/// converges what it already owns and claims nothing new — the safe direction, since the
/// alternative is claiming work for a pool this box may not have.
async fn node_roles(client: &kube::Client, node: &str) -> Vec<String> {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    let Ok(Some(n)) = api.get_opt(node).await else {
        tracing::warn!(%node, "could not read this node's labels: claiming no unplaced work");
        return vec![];
    };
    let labels = n.metadata.labels.unwrap_or_default();
    let roles: Vec<String> = ["session", "env"]
        .into_iter()
        .filter(|r| labels.get(&format!("rustic-git.io/{r}")).map(String::as_str) == Some("true"))
        .map(str::to_string)
        .collect();
    if roles.is_empty() {
        // Zero roles means zero claim watches, and an agent with no claim watch looks identical to
        // a healthy one from the outside — it just never picks anything up. Say so.
        tracing::warn!(%node, "no rustic-git.io/session or /env label: this node claims no unplaced work");
    }
    roles
}

/// Local storage janitor: every `WSSNAP_JANITOR_SECS` (default 600), reclaims local disk that a
/// pushed history no longer needs. Retention
/// rule: PUSHED history is re-derivable from the registry at any time (blobs are immutable
/// there), so a pushed local snapshot is pure cache — reclaimed once it's neither the tip (the
/// parent `commit_core`'s `btrfs send -p` needs for the NEXT delta) nor the current block-layer
/// base (the snapshot name `Engine::squash_inner`'s graft-after-race logic still looks up by
/// name while a squash is in flight). Unpushed anything is the ONLY local copy of that data and
/// is never touched — this whole function skips any lineage entry still marked `unpushed`. Stage
/// files and block images additionally get an age floor (`SWEEP_MIN_AGE`), because a push in
/// flight has both on disk before any lineage entry names them.
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
            let staged = janitor_sweep_stage(&engine, &unpushed_blobs, SWEEP_MIN_AGE);
            let images = janitor_sweep_images(&engine, SWEEP_MIN_AGE);
            if reclaimed > 0 || staged > 0 || images > 0 {
                tracing::info!(reclaimed, staged, images, "agent: janitor reclaimed snapshot(s), stray stage file(s), block image(s)");
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

/// A stage file (and a stray block image) is only ever swept as ORPHAN garbage — a crash leftover
/// — so anything younger than this is presumed to belong to work still in flight and left alone.
/// `Engine::commit_core` writes the staged blob BEFORE appending its `unpushed` lineage entry, and
/// this sweep builds its keep-set from lineage files alone: without the floor, a tick landing in
/// that window deletes the only copy of freshly staged data and the retried push then fails
/// forever on the missing stage file. An age floor rather than `ws_lock`: the stage dir is
/// pool-global while the flock is per-volume (the janitor would have to hold every volume's lock
/// at once), the janitor runs on the shared reactor where a blocking flock stalls every other
/// task, and the lock still wouldn't close the window — the file exists before anything the sweep
/// can observe. Reclaiming an hour late costs disk; reclaiming a second early costs data.
const SWEEP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// True when `entry` is younger than `min_age`. An unreadable mtime counts as young: keeping a
/// file costs disk, deleting one costs data — the sweep never guesses in the delete direction.
fn younger_than(entry: &std::fs::DirEntry, min_age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < min_age).unwrap_or(true))
        .unwrap_or(true)
}

/// Removes any staged layer/meta file (`{blob}.zst`/`{blob}.json` under `Pool::stage_dir`) whose
/// blob id isn't in `keep` and which is older than `min_age` — orphaned by a crash between
/// staging and push clearing it, since a clean push already deletes its own. Global (not
/// per-volume): the stage dir is shared pool state, so `keep` must already be the union across
/// every volume's unpushed entries.
fn janitor_sweep_stage(engine: &Engine, keep: &std::collections::HashSet<String>, min_age: std::time::Duration) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.stage_dir()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        if keep.contains(&stem) || younger_than(&entry, min_age) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// Whether `img` is currently backing a loop device — the only state that makes a block image
/// irreplaceable locally (it is the live filesystem under a block-restored voldir). Everything
/// else in `{pool}/img` is re-fetchable from the object store, the same "pushed bytes are pure
/// cache" rule the snapshot sweep already applies.
fn loop_attached(img: &std::path::Path) -> bool {
    match std::process::Command::new("losetup").arg("-j").arg(img).output() {
        Ok(out) => !out.stdout.is_empty(),
        // No losetup, or it failed: assume attached and keep the file.
        Err(_) => true,
    }
}

/// Reclaims `{pool}/img/*.img` left behind by a squash that died before its own delete, or by a
/// block-restore whose voldir has since been unmounted. Deliberately NOT keyed on "referenced by
/// a lineage": a squash's block image is referenced by the very lineage it creates and is still
/// disposable the moment its bytes are in the object store, so that rule would reclaim nothing.
/// Age floor as in `janitor_sweep_stage`: a restore streams its image to disk BEFORE mounting it,
/// so a young unattached image is a materialization in flight, not garbage.
fn janitor_sweep_images(engine: &Engine, min_age: std::time::Duration) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.img_dir()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        if younger_than(&entry, min_age) || loop_attached(&p) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// `Engine::push`'s detached `squash <ws-id>` child (`ops.rs`) is spawned with only the
/// workspace id, no owner — so the id -> owner mapping has to be recoverable locally.
/// `MetaStore` has no owner-less lookup (Cosmos partitions by owner), so the controller leaves a
/// breadcrumb on the pool itself, right where the lineage file already lives.
pub fn owner_file(pool: &str, ws_id: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("vol").join(format!("{ws_id}.owner"))
}

fn record_owner(pool: &str, id: &str, owner: &str) {
    let _ = std::fs::create_dir_all(std::path::Path::new(pool).join("vol"));
    let _ = std::fs::write(owner_file(pool, id), owner);
}

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

/// Full local reclaim for a deleted workspace/environment: the live subvolume, every RO snapshot
/// its local lineage names, staged (still-unpushed) layer/meta files, the pool's own
/// `.lineage`/`.owner`/`.lock`/`.squash-err` bookkeeping, and finally the `{pool}/vol/{id}`
/// directory itself. Registry/blob bytes are NEVER touched here — blobs are immutable and shared
/// across siblings (a clone's history references the same blob ids), deleted only by an explicit
/// blob-delete path or GC, never by a workspace/environment delete. Best-effort throughout
/// (eprintln, never fails): a retried delete must still finish even if a prior attempt got
/// partway through.
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
            tracing::warn!(%id, path = %voldir.display(), error = %e, "agent: cleanup: remove");
        }
    }
}

fn btrfs_delete(path: &std::path::Path, id: &str) {
    match std::process::Command::new("btrfs").args(["subvolume", "delete", path.to_str().unwrap()]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            %id,
            path = %path.display(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            "agent: cleanup: btrfs subvolume delete"
        ),
        Err(e) => tracing::warn!(%id, path = %path.display(), error = %e, "agent: cleanup: btrfs subvolume delete"),
    }
}

/// `stop`/`start` by exact container name — distinct from `container::stop`, which derives the
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

    fn bare_engine(pool_root: std::path::PathBuf) -> Engine {
        Engine::new(
            Pool::new(pool_root),
            std::sync::Arc::new(object_store::memory::InMemory::new()),
            std::sync::Arc::new(MemStore::new()),
            rustic_git_workspaces::registry_client::RegistryClient::new("http://127.0.0.1:1", "unused"),
        )
    }

    /// The H6b race, reproduced without btrfs: `commit_core` has written the staged blob but not
    /// yet appended its lineage entry, so the keep-set legitimately does not name it. A janitor
    /// tick in that window must not delete the only copy of that data.
    #[test]
    fn stage_sweep_spares_a_file_staged_seconds_ago_with_no_lineage_entry_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        std::fs::write(engine.pool.stage_path("mid-push"), b"layer bytes").unwrap();
        std::fs::write(engine.pool.stage_meta_path("mid-push"), b"{}").unwrap();

        let keep = std::collections::HashSet::new();
        assert_eq!(janitor_sweep_stage(&engine, &keep, SWEEP_MIN_AGE), 0, "a young stage file is presumed live");
        assert!(engine.pool.stage_path("mid-push").exists());
        assert!(engine.pool.stage_meta_path("mid-push").exists());
    }

    /// The other half of the contract: past the floor, a genuine orphan is still reclaimed.
    #[test]
    fn stage_sweep_still_reclaims_an_old_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        let p = engine.pool.stage_path("crashed-push");
        std::fs::write(&p, b"orphan").unwrap();

        assert_eq!(janitor_sweep_stage(&engine, &std::collections::HashSet::new(), std::time::Duration::ZERO), 1);
        assert!(!p.exists());
    }

    /// Crash simulation for the two data-loss paths together: an empty `.lineage` (what a
    /// truncate-then-write crash used to leave) yields an empty keep-set, and the sweep must
    /// STILL not delete the staged blobs that lineage was supposed to name.
    #[test]
    fn an_empty_lineage_file_does_not_let_the_sweep_delete_staged_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.root.join("vol").join("v1")).unwrap();
        std::fs::write(engine.pool.root.join("vol").join("v1.lineage"), b"").unwrap();
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        std::fs::write(engine.pool.stage_path("b1"), b"only copy").unwrap();

        let keep: std::collections::HashSet<String> =
            engine.pool.lineage("v1").iter().filter(|e| e.unpushed).map(|e| e.blob.clone()).collect();
        assert!(keep.is_empty(), "a truncated lineage really does yield an empty keep-set");
        assert_eq!(janitor_sweep_stage(&engine, &keep, SWEEP_MIN_AGE), 0);
        assert!(engine.pool.stage_path("b1").exists(), "unpushed data survives a truncated lineage");
    }

    /// `losetup` doesn't exist on this Mac, so `loop_attached` fails closed (keeps everything) —
    /// which is exactly the behaviour worth freezing on the delete-safety side. The age floor is
    /// tested on its own, since it is the half that decides on Linux too.
    #[test]
    fn image_sweep_keeps_young_images_and_reclaims_old_unattached_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.img_dir()).unwrap();
        let img = engine.pool.img("blob-1");
        std::fs::write(&img, b"image bytes").unwrap();

        assert_eq!(janitor_sweep_images(&engine, SWEEP_MIN_AGE), 0, "a young image is a restore in flight");
        assert!(img.exists());

        // Past the floor: reclaimed unless something still has it looped.
        let swept = janitor_sweep_images(&engine, std::time::Duration::ZERO);
        if loop_attached(&img) {
            assert_eq!(swept, 0, "an attached (or unprobeable) image is never deleted");
            assert!(img.exists());
        } else {
            assert_eq!(swept, 1);
            assert!(!img.exists());
        }
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
        lp.pool.set_lineage(id, &lineage).unwrap();
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
