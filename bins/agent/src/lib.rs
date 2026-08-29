//! Process setup for `rustic-git-agent`: the local `Engine`, the storage janitor, and the
//! Kubernetes client the node controller (`controller.rs`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.

use rustic_git_workspaces::engine::{blob, Engine, Pool};
use rustic_git_workspaces::model::{LayerKind, LineageEntry};
use rustic_git_workspaces::store::MetaStore;
use std::sync::Arc;

pub mod binding;
pub mod claim;
pub mod controller;
pub mod nix;
pub mod snapshot;
pub mod sshkeys;

/// Env-derived config shared by `run` and the `squash` subcommand — both need the same Engine.
pub struct Config {
    pub api_url: String,
    pub region: String,
    pub agent_token: String,
    pub pool: String,
    /// This node's name, from the downward-API `NODE_NAME`. It is the shard key: the controller
    /// watches only objects whose `spec.nodeName` equals it.
    pub node: String,
}

impl Config {
    /// Base URL for the agent work surface: `WS_REGISTRY_URL` names the SERVER tier, not
    /// `bins/api`, which is where register/work/done/failed live.
    pub fn from_env() -> Config {
        let api_url = match std::env::var("WS_REGISTRY_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => "http://127.0.0.1:8081".into(),
        };
        Config {
            api_url,
            region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
            agent_token: std::env::var("WS_AGENT_TOKEN").unwrap_or_default(),
            pool: std::env::var("WS_POOL").unwrap_or_else(|_| "/mnt/wspool".into()),
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
    let nix_client: Arc<dyn nix::Nix> = Arc::new(nix::RealNix { bin: "/nix/var/nix/profiles/default/bin".into() });
    spawn_janitor(engine.clone(), cfg.pool.clone(), nix_client.clone());
    if cfg.node.is_empty() {
        return Err("NODE_NAME is unset: the controller would watch every node's objects".into());
    }
    let pin = nix::nixpkgs_pin();
    if pin.is_empty() {
        return Err("WS_NIXPKGS is required: the nixpkgs pin every profile on this node is built against".into());
    }
    // A branch or a tag would make the same package hash mean different bits on different days,
    // which is the one promise the profile hash makes.
    if !nix::valid_pin(&pin) {
        return Err(format!("WS_NIXPKGS must be github:NixOS/nixpkgs/<40-hex-rev>, not {pin:?}"));
    }
    if let Err(e) = std::fs::create_dir_all(nix::PROFILES_DIR) {
        tracing::warn!(error = %e, "could not create the Nix profiles dir — the daemon container seeds /nix");
    }
    // One indirect root over the whole profiles tree: `nix build --no-link` registers none, and
    // the publish rename would orphan an out-link's auto-root anyway.
    nix::ensure_gcroot();
    // The CRDs must be Established before the watch starts, or it fails at startup and the
    // controller sits idle looking healthy. Fail loudly here rather than in production.
    let client = kube::Client::try_default().await.map_err(|e| e.to_string())?;
    let roles = node_roles(&client, &cfg.node).await;
    tracing::info!(node = %cfg.node, ?roles, "node roles");
    let ctx = Arc::new(controller::Ctx::new(client, engine, cfg.node, cfg.pool, cfg.region, roles, nix_client, nix::PROFILES_DIR.into(), Arc::new(sshkeys::SshKeygen)));
    controller::run(ctx).await
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

/// Local storage janitor: every ten minutes, reclaims local disk that a
/// pushed history no longer needs. Retention
/// rule: PUSHED history is re-derivable from the registry at any time (blobs are immutable
/// there), so a pushed local snapshot is pure cache — reclaimed once it's neither the tip (the
/// parent `commit_core`'s `btrfs send -p` needs for the NEXT delta) nor the current block-layer
/// base (the snapshot name `Engine::squash_inner`'s graft-after-race logic still looks up by
/// name while a squash is in flight). Unpushed anything is the ONLY local copy of that data and
/// is never touched — this whole function skips any lineage entry still marked `unpushed`. Stage
/// files and block images additionally get an age floor (`SWEEP_MIN_AGE`), because a push in
/// flight has both on disk before any lineage entry names them.
///
/// The whole beat runs on ONE blocking thread: every step shells out to `btrfs`/`losetup` or walks
/// a directory, and on the reactor each of those stalled every in-flight reconcile for as long as
/// a subvolume delete takes — hundreds of volumes on a two-vCPU node is minutes of that, aligned
/// to the ten-minute interval.
fn spawn_janitor(engine: Arc<Engine>, pool: String, nix: Arc<dyn nix::Nix>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(600));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let (engine, pool) = (engine.clone(), pool.clone());
            let beat = tokio::task::spawn_blocking(move || {
                let (reclaimed, staged, images) = janitor_beat(&engine, &pool);
                if reclaimed > 0 || staged > 0 || images > 0 {
                    tracing::info!(reclaimed, staged, images, "agent: janitor reclaimed snapshot(s), stray stage file(s), block image(s)");
                }
                // The store is a per-node cache; the profile out-links are its only roots, so a
                // GC is always safe and the only question is when. Size by `du` of the store dir,
                // best effort — a wrong number costs an early or late GC, never data.
                // ponytail: du of a 60 GB store every 10 min is real IO; `statvfs` of the /nix
                // filesystem is the cheaper signal once /nix is its own mount.
                nix_store_bytes(std::path::Path::new("/nix/store"))
            })
            .await;
            // The sweep is blocking (it shells out to `btrfs`); the GC is not — `nix` is driven
            // through tokio — so only the sweep goes to a blocking thread.
            match beat {
                Ok(used) if used > NIX_GC_HIGH_BYTES => match nix.collect_garbage().await {
                    Ok(freed) => tracing::info!(used, freed, "agent: nix store over threshold, collected garbage"),
                    Err(e) => tracing::warn!(error = %e, "agent: nix-collect-garbage failed"),
                },
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "agent: the janitor beat panicked; skipping it"),
            }
        }
    });
}

/// One sweep of the pool: (snapshots reclaimed, stage files swept, block images swept).
///
/// Every lineage file is read ONCE, into `lineages`, and the cross-volume facts the sweeps need
/// (which snapshot names more than one volume shares, which blobs are still unpushed anywhere)
/// are derived from that map — the per-volume sweep used to re-read every OTHER lineage for each
/// volume, which is V² file reads per beat.
fn janitor_beat(engine: &Engine, pool: &str) -> (usize, usize, usize) {
    let lineages = read_lineages(std::path::Path::new(pool), engine);
    let shared = shared_snap_names(&lineages);
    // A blob referenced by ANY volume's still-unpushed lineage entry must survive the global stage
    // sweep, even though the stage dir isn't scoped per volume.
    let unpushed_blobs: std::collections::HashSet<String> =
        lineages.values().flatten().filter(|e| e.unpushed).map(|e| e.blob.clone()).collect();
    let named: std::collections::HashSet<String> = lineages.values().flatten().map(|e| e.snap_name().to_string()).collect();
    let mut reclaimed = 0;
    for (id, lineage) in &lineages {
        reclaimed += janitor_volume_snapshots(engine, id, lineage, &shared);
    }
    reclaimed += janitor_sweep_recv(engine, &named, SWEEP_MIN_AGE);
    let staged = janitor_sweep_stage(engine, &unpushed_blobs, SWEEP_MIN_AGE);
    let images = janitor_sweep_images(engine, SWEEP_MIN_AGE);
    (reclaimed, staged, images)
}

/// Every volume on the pool with its lineage, from one `read_dir` of `{pool}/vol`.
fn read_lineages(pool: &std::path::Path, engine: &Engine) -> std::collections::HashMap<String, Vec<LineageEntry>> {
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(pool.join("vol")) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        let lineage = engine.pool.lineage(&id);
        out.insert(id, lineage);
    }
    out
}

/// The snapshot names that appear in MORE THAN ONE volume's lineage. A local-first clone
/// (`Engine::clone_local_snapshot`) copies the source's lineage VERBATIM, so one `recv/{snap}` can
/// be the source's history AND a clone's tip or `btrfs send -p` parent at once — reclaiming it for
/// one breaks the other's next push. Counted per volume, not per entry, so a lineage naming the
/// same snapshot twice does not make it "shared" with itself.
fn shared_snap_names(lineages: &std::collections::HashMap<String, Vec<LineageEntry>>) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for lineage in lineages.values() {
        let names: std::collections::HashSet<&str> = lineage.iter().map(|e| e.snap_name()).collect();
        for n in names {
            *seen.entry(n.to_string()).or_default() += 1;
        }
    }
    seen.into_iter().filter(|(_, n)| *n > 1).map(|(name, _)| name).collect()
}

/// The store size past which the janitor triggers a `nix-collect-garbage` sweep.
const NIX_GC_HIGH_BYTES: u64 = 60 * 1024 * 1024 * 1024;

/// Recursive size of `root`, best effort: an unreadable entry is skipped rather than failing the
/// whole scan, since a wrong number only costs an early or late GC, never data. Uses
/// `DirEntry::file_type` (an `lstat`, not a `stat`) so it never follows a symlink — `/nix/store`
/// is full of symlinks between store paths, and following them would double-count shared files
/// and could cycle forever on a symlink back up the tree.
fn nix_store_bytes(root: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            total += nix_store_bytes(&entry.path());
        } else if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
        // symlinks: skip — not real bytes owned by this dir, and following one risks a cycle.
    }
    total
}

/// Snapshot-reclaim pass for one volume's lineage, split out of `spawn_janitor`'s loop so it can
/// be exercised directly by a test without waiting on the interval. Never touches staged files
/// (that's `janitor_sweep_stage`'s job, done once globally per tick, not per volume).
///
/// `shared` is `shared_snap_names` over the whole pool: a snapshot that's a non-tip, already-pushed
/// entry for THIS volume can still be another volume's tip or `btrfs send -p` parent — reclaiming
/// it here would break that sibling's next push. Same cross-volume rule `cleanup_local` applies
/// before a delete.
fn janitor_volume_snapshots(engine: &Engine, id: &str, lineage: &[LineageEntry], shared: &std::collections::HashSet<String>) -> usize {
    let Some(tip) = lineage.last() else { return 0 };
    let tip_name = tip.snap_name().to_string();
    let block_base = lineage.iter().rev().find(|e| e.kind == LayerKind::Block).map(|e| e.snap_name().to_string());
    let root = engine.pool.snap_root(id);
    let mut reclaimed = 0;
    for e in lineage {
        if e.unpushed {
            continue;
        }
        let name = e.snap_name();
        if name == tip_name || Some(name) == block_base.as_deref() || shared.contains(name) {
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

/// Reclaims `recv/*` subvolumes no lineage on this pool names. `janitor_volume_snapshots` walks
/// lineages, so a snapshot that never made it INTO one — `commit_core` takes it before the send
/// and appends the entry only after — is invisible to it and pins its extents forever after a
/// crash in that window. Age floor as in `janitor_sweep_stage`, for the same reason: a snapshot
/// whose send is still running is exactly such an unnamed one. The subvolume's own creation time,
/// NOT the directory mtime — a snapshot inherits the mtime of the tree it was taken from, which
/// can be months old the moment it is created. Unknown age keeps.
///
/// `named` is every snapshot name any lineage on the pool carries, from the beat's one read.
fn janitor_sweep_recv(engine: &Engine, named: &std::collections::HashSet<String>, min_age: std::time::Duration) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.recv()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if named.contains(&name) || !subvolume_older_than(&p, min_age) {
            continue;
        }
        btrfs_delete(&p, "recv");
        swept += 1;
    }
    swept
}

/// `btrfs subvolume show`'s `Creation time`, compared against `min_age`. Anything that is not a
/// subvolume, or whose age cannot be read, is "not old enough" — the sweep never guesses in the
/// delete direction.
fn subvolume_older_than(p: &std::path::Path, min_age: std::time::Duration) -> bool {
    let Ok(out) = std::process::Command::new("btrfs").args(["subvolume", "show"]).arg(p).output() else { return false };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(line) = text.lines().find(|l| l.trim_start().starts_with("Creation time:")) else { return false };
    let stamp = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or_default();
    let Ok(created) = chrono::DateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S %z") else { return false };
    let age = chrono::Utc::now().signed_duration_since(created);
    age.to_std().is_ok_and(|a| a >= min_age)
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

/// Every file currently backing a loop device — the only state that makes a block image
/// irreplaceable locally (it is the live filesystem under a block-restored voldir). Everything
/// else in `{pool}/img` is re-fetchable from the object store, the same "pushed bytes are pure
/// cache" rule the snapshot sweep already applies.
///
/// ONE `losetup -l -J` for the whole sweep, not one `-j` per image. `None` when losetup is
/// missing, fails, or answers something unparseable: the caller then keeps every image.
fn attached_images() -> Option<std::collections::HashSet<std::path::PathBuf>> {
    let out = std::process::Command::new("losetup").args(["-l", "-J"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        v.get("loopdevices")?
            .as_array()?
            .iter()
            .filter_map(|d| d.get("back-file")?.as_str())
            // util-linux marks a backing file it can no longer see with a trailing " (deleted)".
            .map(|f| std::path::PathBuf::from(f.trim_end_matches(" (deleted)")))
            .collect(),
    )
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
    // Unprobeable means attached: the sweep never guesses in the delete direction.
    let Some(attached) = attached_images() else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        if younger_than(&entry, min_age) || attached.contains(&p) {
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
    // One scan of `{pool}/vol`, two projections of every OTHER volume's lineage:
    //
    // `elsewhere` — a local-first clone (`Engine::clone_local`) shares its inherited unpushed
    // entries' staged files with the source by blob id (`Pool::stage_dir` is pool-global) rather
    // than copying them, so deleting the source must not strip a stage file a sibling clone still
    // needs to push. Same scan `spawn_janitor`'s stage sweep uses, just excluding this volume.
    //
    // `elsewhere_snaps` — the same sharing one level up: `clone_local_snapshot` copies the
    // source's lineage VERBATIM, so `recv/{snap}` can be BOTH this volume's own history AND a
    // clone's tip/parent at once; deleting it here would leave the clone's next push sending `-p`
    // against a snapshot that no longer exists (the real bug this scan closes).
    // ponytail: one `vol/` scan per delete; fine at expected per-pool volume counts.
    let others = read_lineages(&engine.pool.root, engine);
    let others = others.iter().filter(|(other, _)| other.as_str() != id).flat_map(|(_, l)| l);
    let (mut elsewhere, mut elsewhere_snaps) =
        (std::collections::HashSet::new(), std::collections::HashSet::new());
    for e in others {
        if e.unpushed {
            elsewhere.insert(e.blob.clone());
        }
        elsewhere_snaps.insert(e.snap_name().to_string());
    }
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

    /// `losetup` doesn't exist on this Mac, so `attached_images` fails closed (keeps everything) —
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

        // Past the floor: reclaimed unless something still has it looped, or nothing can say.
        let swept = janitor_sweep_images(&engine, std::time::Duration::ZERO);
        match attached_images() {
            Some(attached) if !attached.contains(&img) => {
                assert_eq!(swept, 1);
                assert!(!img.exists());
            }
            _ => {
                assert_eq!(swept, 0, "an attached (or unprobeable) image is never deleted");
                assert!(img.exists());
            }
        }
    }

    /// The O(V) half of the beat: one read of every lineage, and "shared" is a snapshot named by
    /// two DIFFERENT volumes — a clone's verbatim copy — never one a single lineage repeats.
    #[test]
    fn shared_snapshots_are_the_ones_two_volumes_name() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        for id in ["src", "clone", "loner"] {
            std::fs::create_dir_all(engine.pool.voldir(id)).unwrap();
        }
        engine.pool.set_lineage("src", &[stream_entry("s1", false), stream_entry("s2", false)]).unwrap();
        engine.pool.set_lineage("clone", &[stream_entry("s1", false), stream_entry("s1", false), stream_entry("c1", true)]).unwrap();
        engine.pool.set_lineage("loner", &[stream_entry("l1", false), stream_entry("l1", false)]).unwrap();
        // A stray file under vol/ is not a volume.
        std::fs::write(engine.pool.root.join("vol").join("notes.txt"), b"").unwrap();

        let lineages = read_lineages(&engine.pool.root, &engine);
        assert_eq!(lineages.len(), 3);
        let shared = shared_snap_names(&lineages);
        assert_eq!(shared, std::collections::HashSet::from(["s1".to_string()]), "{shared:?}");
    }

    /// Q-37's other half: a snapshot `commit_core` took and then crashed before naming is in no
    /// lineage, so only a sweep of `recv/` itself finds it. The floor is the subvolume's own age —
    /// a snapshot inherits its tree's mtime, which proves nothing about when it was taken.
    #[test]
    fn recv_sweep_reclaims_only_old_snapshots_no_lineage_names() {
        if !have_btrfs() {
            eprintln!("skipping: btrfs unavailable or not root");
            return;
        }
        let lp = LoopbackPool::new();
        for s in ["named", "orphan"] {
            run(&["btrfs", "subvolume", "create", lp.pool.recv().join(s).to_str().unwrap()]);
        }
        std::fs::create_dir_all(lp.pool.voldir("vol-recv-1")).unwrap();
        lp.pool.set_lineage("vol-recv-1", &[stream_entry("named", false)]).unwrap();
        let engine = bare_engine(lp.pool.root.clone());
        let named = std::collections::HashSet::from(["named".to_string()]);

        assert_eq!(janitor_sweep_recv(&engine, &named, SWEEP_MIN_AGE), 0, "a young orphan is a send in flight");
        assert!(lp.pool.recv().join("orphan").exists());

        assert_eq!(janitor_sweep_recv(&engine, &named, std::time::Duration::ZERO), 1);
        assert!(!lp.pool.recv().join("orphan").exists());
        assert!(lp.pool.recv().join("named").exists(), "a snapshot any lineage names is never touched");
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
        let reclaimed = janitor_volume_snapshots(&engine, id, &lineage, &std::collections::HashSet::new());
        assert_eq!(reclaimed, 3, "the 3 pushed non-tip snapshots must be reclaimed");

        assert!(!lp.pool.recv().join("s1").exists());
        assert!(!lp.pool.recv().join("s2").exists());
        assert!(!lp.pool.recv().join("s3").exists());
        assert!(lp.pool.recv().join("s4").exists(), "the unpushed tip must never be touched");
        assert!(lp.pool.stage_meta_path("s4").exists(), "unpushed stage files must be left intact");
    }
}

#[cfg(test)]
mod nix_gc_tests {
    use super::*;

    #[test]
    fn the_store_walk_does_not_follow_symlinks_and_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("f"), vec![0u8; 100]).unwrap();
        // A symlink back up at the root — following it would double-count `f` and, on a deeper
        // tree, cycle forever.
        std::os::unix::fs::symlink(root, nested.join("up")).unwrap();
        assert_eq!(nix_store_bytes(root), 100);
    }
}
