//! Engine operations: push, pull, clone_local, clone_running, restore, squash. Ported from
//! `docs/superpowers/poc/wssnap/main.rs` (Azure-tested). `push` is the one user-facing mutating
//! verb: a local RO snapshot + lineage append (staged, marked `unpushed`), immediately followed
//! by uploading every unpushed entry's staged blob, POSTing their `CommitRecord`s to the volume
//! registry (`registry_client`), moving the registry ref, and clearing the marks — snapshot and
//! upload happen atomically from the caller's point of view. The two-phase shape survives only
//! internally, as the crash-recovery seam: if a push dies between staging and the registry call
//! landing, the stage files and `unpushed` marks are left in place so a retried push picks them
//! up rather than losing them or re-snapshotting. `push` also decides whether to auto-squash.
//!
//! `MetaStore`'s `put_snapshot`/`get_snapshot`/`Workspace.ref_`/`Environment.ref_` are no longer
//! read or written here (they're `fsck`'s recovery surface now, untouched by this module, and
//! the Cosmos `ref`/`volume` pointer on the workspace/environment doc itself is updated by the
//! job-done handler, not the engine). Lineage truth lives in two places: the local
//! `{pool}/vol/{id}.lineage` file (this pool's view, `unpushed`-tagged) and the registry's
//! `commit`/`ref` keyspace (durable, shared).
//!
//! Two ways a local copy gets made, picked by the caller (`bins/agent/src/lib.rs`'s `WsClone`
//! arm) on whether the source's container is running: `clone_local` for a stopped/never-pushed
//! source, `clone_running` for a live one. Both then route AGAIN on the same locality signal
//! (`src`'s live subvolume materialized on this pool, which a running container always implies):
//! local-first when true — no registry call, no push-first requirement, works on a source that's
//! never snapshotted at all — and only falls back to the registry-prefetch path (which DOES need
//! `src` to have pushed) when `src` genuinely lives elsewhere. `clone_local` was local-first from
//! the start; `clone_running` grew the same split after "clone a running, never-pushed workspace"
//! shipped broken — the registry path was the only one it had, so it hit `inherit`'s "clone
//! source has no snapshots; push first" for a case that never touches the network at all now.
//! Both back the one user-facing route, `POST /v1/workspaces/{id}/clone`.
//!
//! Clone semantics changed with the split: there is no more `copy_ref` duplicating a
//! `Snapshot` doc under the destination's id. Instead `clone_local` reads the source's history
//! from the registry, materializes it locally, and stages every inherited entry as `unpushed`
//! under the DESTINATION's id — the blobs are already in the shared object store (no re-upload),
//! but the destination's own `{owner}/{name}` commit/ref keyspace on the registry is empty until
//! its first `push` writes fresh `CommitRecord`s there and moves its own ref. A cloned
//! workspace that is never pushed has no registry history of its own — `pull` on it fails
//! clean, same as any workspace that's never been pushed.

use crate::engine::{Pool, blob, is_mountpoint, ws_lock};
use crate::model::{LayerKind, LineageEntry, Workspace};
use crate::registry::CommitRecord;
use crate::registry_client::{MAIN_REF, RegistryClient};
use crate::store::MetaStore;
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as S3Path};
use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct EngErr(pub String);

impl std::fmt::Display for EngErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for EngErr {}
impl From<String> for EngErr {
    fn from(s: String) -> Self {
        EngErr(s)
    }
}
impl EngErr {
    pub(crate) fn store(e: crate::store::StoreErr) -> Self {
        EngErr(format!("{e:?}"))
    }
    pub(crate) fn io(e: std::io::Error) -> Self {
        EngErr(e.to_string())
    }
    pub(crate) fn other(s: impl Into<String>) -> Self {
        EngErr(s.into())
    }
}

#[derive(Debug)]
pub struct PushOut {
    pub layer: String,
    pub sha: String,
    pub raw: u64,
    pub compressed: u64,
    pub layers: usize,
    pub squash_triggered: Option<String>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct PullOut {
    pub layers: usize,
    pub fetched: usize,
}

#[derive(Debug)]
pub struct CloneOut {
    pub prefetched: Duration,
    pub locked: Duration,
    pub total: Duration,
}

/// What `commit` stages locally next to the (optional) compressed blob at `Pool::stage_path` —
/// everything `push` needs to build the `CommitRecord` later without recomputing anything.
/// `raw`/`clen` are 0 and no sibling `.zst` exists for an entry `push` only needs to REGISTER,
/// not upload (a squash's block layer, already put to the store directly; an inherited
/// clone entry, already in the store under the source's push) — `push_core` tells the two
/// apart by whether `Pool::stage_path` exists.
#[derive(serde::Serialize, serde::Deserialize)]
struct StageMeta {
    raw: u64,
    clen: u64,
    #[serde(default)]
    state: serde_json::Value,
    #[serde(default)]
    message: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap().trim().into()
}

fn run(argv: &[&str]) -> Result<(), EngErr> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| EngErr::other(format!("spawn {}: {e}", argv[0])))?;
    if !out.status.success() {
        return Err(EngErr::other(format!("{argv:?}: {}", String::from_utf8_lossy(&out.stderr))));
    }
    Ok(())
}

fn write_stage_meta(pool: &Pool, blob_id: &str, m: &StageMeta) -> Result<(), EngErr> {
    std::fs::create_dir_all(pool.stage_dir()).map_err(EngErr::io)?;
    let bytes = serde_json::to_vec(m).map_err(|e| EngErr::other(e.to_string()))?;
    std::fs::write(pool.stage_meta_path(blob_id), bytes).map_err(EngErr::io)
}

pub struct Engine {
    pub pool: Pool,
    pub store: Arc<dyn ObjectStore>,
    pub meta: Arc<dyn MetaStore>,
    pub registry: RegistryClient,
    /// Delta size (MB) that forces a block layer. Env `WSSNAP_SQUASH_MB`, default 256.
    pub squash_mb: u64,
    /// Stream layers since the last block layer that force one. Env `WSSNAP_CHAIN_MAX`, default 50.
    pub chain_max: usize,
}

impl Engine {
    pub fn new(pool: Pool, store: Arc<dyn ObjectStore>, meta: Arc<dyn MetaStore>, registry: RegistryClient) -> Engine {
        Engine {
            pool,
            store,
            meta,
            registry,
            squash_mb: std::env::var("WSSNAP_SQUASH_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(256),
            chain_max: std::env::var("WSSNAP_CHAIN_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(50),
        }
    }

    /// Bare `{pool}/vol/{id}/live` subvolume creation — shared by `init` (a workspace, which
    /// pushes immediately after) and `EnvUp`'s first-ever-mount path (an environment, which
    /// doesn't push until `EnvDown`).
    pub fn create_subvol(&self, id: &str) -> Result<(), EngErr> {
        std::fs::create_dir_all(self.pool.voldir(id)).map_err(EngErr::io)?;
        // Reconcile is level-triggered and a restarted controller replays it from scratch, so an
        // existing `live` is the expected steady state, not a conflict. Keep-biased: never delete
        // and recreate — that would be data loss dressed up as convergence. Same guard `pull_core`
        // already applies before its snapshot.
        if !self.pool.live(id).exists() {
            run(&["btrfs", "subvolume", "create", self.pool.live(id).to_str().unwrap()])?;
        }
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        Ok(())
    }

    pub async fn init(&self, ws: &Workspace) -> Result<(), EngErr> {
        self.create_subvol(&ws.id)?;
        self.push(ws, Some("initial")).await?;
        Ok(())
    }

    /// RO snapshot of `id`'s live subvolume, delta-compressed to a LOCAL staging file (never
    /// uploaded here — that's the upload phase's job, below), lineage extended with an entry
    /// marked `unpushed`. Fast and offline: the only IO is local disk (the btrfs snapshot/send
    /// and the zstd compress), no object-store or registry call. Private: the only caller is
    /// `push_core`, immediately followed by the upload phase — nothing user-facing can observe
    /// this step on its own any more.
    async fn commit_core(
        &self,
        id: &str,
        live_state: &serde_json::Value,
        message: Option<&str>,
    ) -> Result<String, EngErr> {
        let _lock = ws_lock(&self.pool, id).map_err(EngErr::other)?;
        let mut lineage = self.pool.lineage(id);
        let root = self.pool.snap_root(id);
        let parent = lineage.last().map(|e| root.join(e.snap_name()));
        let layer_id = uuid();
        run(&[
            "btrfs",
            "subvolume",
            "snapshot",
            "-r",
            self.pool.live(id).to_str().unwrap(),
            root.join(&layer_id).to_str().unwrap(),
        ])?;
        let mut child = blob::spawn_send(&root.join(&layer_id), parent).map_err(EngErr::other)?;
        let dest = self.pool.stage_path(&layer_id);
        let compressed = blob::compress_to_file(child.stdout.take().unwrap(), &dest);
        let st = child.wait_with_output().map_err(EngErr::io)?;
        let (raw, clen, sha) = match (compressed, st.status.success()) {
            (Ok(v), true) => v,
            (res, ok) => {
                let _ = std::fs::remove_file(&dest);
                let mut msg = String::new();
                if !ok {
                    msg.push_str(&format!("btrfs send: {}", String::from_utf8_lossy(&st.stderr)));
                }
                if let Err(e) = res {
                    if !msg.is_empty() {
                        msg.push_str("; ");
                    }
                    msg.push_str(&e);
                }
                return Err(EngErr::other(msg));
            }
        };
        write_stage_meta(
            &self.pool,
            &layer_id,
            &StageMeta { raw, clen, state: live_state.clone(), message: message.map(str::to_string), created_at: chrono::Utc::now() },
        )?;
        lineage.push(LineageEntry { kind: LayerKind::Stream, blob: layer_id.clone(), snap: None, sha256: sha, unpushed: true });
        self.pool.set_lineage(id, &lineage).map_err(EngErr::other)?;
        Ok(layer_id)
    }

    /// Uploads every unpushed lineage entry under `{owner}/{id}` (in lineage order), building
    /// one `CommitRecord` per entry — its `lineage` field is the full prefix up to and including
    /// itself, matching `registry::CommitRecord`'s "never depends on another record" contract.
    /// An entry whose `Pool::stage_path` doesn't exist is registration-only (its bytes are
    /// already in the object store from elsewhere — a squash's block layer, or an
    /// inherited-clone entry): `push` skips the upload and sidecar write for those,
    /// but still POSTs their record and includes them in the ref move. Batches every record
    /// into one `POST .../commits` call, then one ref move — not one round trip per entry.
    /// Private: `push`/`push_env` call this right after `commit_core` stages a fresh entry;
    /// `squash_inner` calls it directly (it stages its own block entry, so a second fresh
    /// snapshot on top would be wrong).
    async fn upload_core(&self, owner: &str, id: &str) -> Result<PushOut, EngErr> {
        let _lock = ws_lock(&self.pool, id).map_err(EngErr::other)?;
        let mut lineage = self.pool.lineage(id);
        let unpushed_idx: Vec<usize> = lineage.iter().enumerate().filter(|(_, e)| e.unpushed).map(|(i, _)| i).collect();
        if unpushed_idx.is_empty() {
            return Err(EngErr::other("nothing staged to push"));
        }
        let t = Instant::now();
        let mut records = Vec::with_capacity(unpushed_idx.len());
        let mut total_raw = 0u64;
        let (mut last_layer, mut last_sha, mut last_clen) = (String::new(), String::new(), 0u64);

        for &i in &unpushed_idx {
            let blob_id = lineage[i].blob.clone();
            let meta_bytes = std::fs::read(self.pool.stage_meta_path(&blob_id)).map_err(EngErr::io)?;
            let meta: StageMeta = serde_json::from_slice(&meta_bytes).map_err(|e| EngErr::other(e.to_string()))?;
            let staged = self.pool.stage_path(&blob_id);
            if staged.exists() {
                let key = format!("layers/{blob_id}.zst");
                blob::upload_file(self.store.as_ref(), &key, &staged).await.map_err(EngErr::other)?;
                let parent_blob = if i > 0 { Some(lineage[i - 1].blob.clone()) } else { None };
                blob::write_sidecar(
                    self.store.as_ref(),
                    &blob_id,
                    &blob::LayerSidecar {
                        kind: lineage[i].kind,
                        parent_blob,
                        snap_uuid: lineage[i].snap.clone(),
                        sha256: lineage[i].sha256.clone(),
                        raw: meta.raw,
                        stored: meta.clen,
                        created_at: meta.created_at,
                    },
                )
                .await
                .map_err(EngErr::other)?;
            }

            let prefix: Vec<LineageEntry> = lineage[..=i]
                .iter()
                .map(|e| LineageEntry { unpushed: false, ..e.clone() })
                .collect();
            records.push(CommitRecord {
                id: blob_id.clone(),
                state: meta.state,
                lineage: prefix,
                region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
                message: meta.message,
                created_at: meta.created_at,
            });
            total_raw += meta.raw;
            last_layer = blob_id;
            last_sha = lineage[i].sha256.clone();
            last_clen = meta.clen;
        }

        self.registry.post_commits(owner, id, &records).await.map_err(EngErr::other)?;
        self.registry.move_ref(owner, id, MAIN_REF, &last_layer).await.map_err(EngErr::other)?;
        // Cleanup only happens once BOTH the records and the ref move are durable — a crash (or
        // a failed post_commits/move_ref) before this point must leave every staged blob/meta
        // file in place, marks still `unpushed`, so a retried push re-uploads (harmless: same
        // blob id, immutable) and re-POSTs (harmless: the registry puts by id) instead of
        // failing forever on a missing stage file.
        for &i in &unpushed_idx {
            let blob_id = &lineage[i].blob;
            let _ = std::fs::remove_file(self.pool.stage_path(blob_id));
            let _ = std::fs::remove_file(self.pool.stage_meta_path(blob_id));
            lineage[i].unpushed = false;
        }
        self.pool.set_lineage(id, &lineage).map_err(EngErr::other)?;

        let since_block = lineage.iter().rev().take_while(|e| e.kind == LayerKind::Stream).count();
        let reason = if total_raw > self.squash_mb << 20 {
            Some(format!("delta > {}MB", self.squash_mb))
        } else if since_block > self.chain_max {
            Some(format!("chain > {}", self.chain_max))
        } else {
            None
        };

        // The latch stops a second squash from spawning while one is still building; the
        // squash child removes it when done.
        let latch = self.squash_latch(id);
        let mut squash_triggered = None;
        if let Some(r) = reason {
            if latch.exists() && !self.latch_is_stale(id) {
                squash_triggered = Some(format!("{r} (already running)"));
            } else {
                std::fs::write(&latch, b"").map_err(EngErr::io)?;
                let exe = std::env::current_exe().map_err(EngErr::io)?;
                std::process::Command::new(exe)
                    .args(["squash", id])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(EngErr::io)?;
                squash_triggered = Some(r);
            }
        }

        Ok(PushOut {
            layer: last_layer,
            sha: last_sha,
            raw: total_raw,
            compressed: last_clen,
            layers: lineage.len(),
            squash_triggered,
            elapsed: t.elapsed(),
        })
    }

    /// The one user-facing mutating verb: snapshot `ws`'s current live subvolume, upload every
    /// unpushed layer (this one plus any left over from a prior crashed push), register their
    /// `CommitRecord`s, move `ws`'s registry ref — atomically from the caller's point of view.
    /// `message` is free-form, carried through to the `CommitRecord`. Auto-squash: the push
    /// itself stays fast (bytes are already durable by the time this returns); the block layer
    /// is built by a detached `rustic-git-agent squash <ws-id>` child.
    /// ponytail: always takes a fresh snapshot, even when `restore`/`inherit` already staged an
    /// unpushed entry and nothing has changed since — a push right after restoring or a
    /// cross-pool clone lands one small, harmless extra record on top of the restored one rather
    /// than detecting "nothing changed" and skipping. Upgrade path if that bloat ever matters:
    /// skip `commit_core` when the tip's `btrfs send -p` delta would be empty AND the lineage
    /// already has unpushed content (the narrow case restore/inherit create), not a blanket
    /// autocommit-style size floor (that swallowed real small writes before and was removed).
    pub async fn push(&self, ws: &Workspace, message: Option<&str>) -> Result<PushOut, EngErr> {
        self.commit_core(&ws.id, &ws.live_state, message).await?;
        self.upload_core(&ws.owner, &ws.id).await
    }

    /// Env variant of `push`, keyed by the env's own id (its one subvolume covers every mounted
    /// volume, so one push captures and lands them all atomically).
    pub async fn push_env(&self, owner: &str, id: &str, live_state: &serde_json::Value, message: Option<&str>) -> Result<PushOut, EngErr> {
        self.commit_core(id, live_state, message).await?;
        self.upload_core(owner, id).await
    }

    /// Materialize `id`'s lineage locally, fetching only what's missing, then point live at
    /// the tip. A lineage whose base is a block layer not yet local restores by image mount:
    /// decompress straight to a loop-mounted fs, no per-file receive for the bulk.
    async fn pull_core(&self, name: &str, lineage: Vec<LineageEntry>) -> Result<PullOut, EngErr> {
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        std::fs::create_dir_all(self.pool.voldir(name)).map_err(EngErr::io)?;

        // Block fast path only when the base isn't already materialized on the shared pool.
        let mut snap_root = self.pool.recv();
        let mut rest = &lineage[..];
        if let Some(first) = lineage.first() {
            if first.kind == LayerKind::Block {
                let snap_name = first.snap_name();
                if !self.pool.recv().join(snap_name).exists() {
                    let wsroot = self.pool.voldir(name);
                    if !is_mountpoint(&wsroot) {
                        // Stream download -> decode -> disk; nothing buffers the whole image.
                        let mut s = self
                            .store
                            .get(&S3Path::from(format!("layers/{}.zst", first.blob)))
                            .await
                            .map_err(|e| EngErr::other(e.to_string()))?
                            .into_stream();
                        std::fs::create_dir_all(self.pool.img_dir()).map_err(EngErr::io)?;
                        let img = self.pool.img(&first.blob);
                        let f = std::fs::File::create(&img).map_err(EngErr::io)?;
                        let mut w = std::io::BufWriter::new(f);
                        let mut dec: Option<zstd::stream::write::Decoder<_>> = None;
                        let mut is_first_chunk = true;
                        let mut h = <sha2::Sha256 as sha2::Digest>::new();
                        while let Some(b) = s.next().await {
                            let b = b.map_err(|e| EngErr::other(e.to_string()))?;
                            sha2::Digest::update(&mut h, &b);
                            let mut d: &[u8] = &b;
                            if is_first_chunk {
                                is_first_chunk = false;
                                let raw_mode = d[0] == b'r';
                                d = &d[1..];
                                if !raw_mode {
                                    dec = Some(
                                        zstd::stream::write::Decoder::new(w).map_err(EngErr::io)?,
                                    );
                                    w = std::io::BufWriter::new(
                                        std::fs::File::open("/dev/null").map_err(EngErr::io)?,
                                    );
                                }
                            }
                            if let Some(dd) = dec.as_mut() {
                                dd.write_all(d).map_err(EngErr::io)?;
                            } else {
                                w.write_all(d).map_err(EngErr::io)?;
                            }
                        }
                        if let Some(mut dd) = dec.take() {
                            dd.flush().map_err(EngErr::io)?;
                        } else {
                            w.flush().map_err(EngErr::io)?;
                        }
                        if blob::sha_hex(h) != first.sha256 {
                            let _ = std::fs::remove_file(&img);
                            return Err(EngErr::other(format!(
                                "block layer {}: sha mismatch (corrupt image)",
                                first.blob
                            )));
                        }
                        run(&["mount", "-o", "loop", img.to_str().unwrap(), wsroot.to_str().unwrap()])?;
                    }
                    snap_root = wsroot;
                    rest = &lineage[1..];
                }
            }
        }

        let missing: Vec<&LineageEntry> = rest.iter().filter(|e| !snap_root.join(e.snap_name()).exists()).collect();
        let mut jobs = Vec::new();
        for e in &missing {
            let store = self.store.clone();
            let key = format!("layers/{}.zst", e.blob);
            jobs.push(tokio::spawn(async move { blob::get_bytes(store.as_ref(), &key).await }));
        }
        let mut blobs = Vec::new();
        for j in jobs {
            blobs.push(j.await.map_err(|e| EngErr::other(e.to_string()))?.map_err(EngErr::other)?);
        }
        for (e, b) in missing.iter().zip(&blobs) {
            let mut h = <sha2::Sha256 as sha2::Digest>::new();
            sha2::Digest::update(&mut h, b);
            let got = blob::sha_hex(h);
            if got != e.sha256 {
                return Err(EngErr::other(format!("layer {}: sha mismatch (corrupt blob)", e.blob)));
            }
            blob::receive_into(&snap_root, b).map_err(EngErr::other)?; // order matters: receive validates the parent-UUID chain
        }
        let tip = lineage.last().ok_or_else(|| EngErr::other("empty lineage"))?;
        if !self.pool.live(name).exists() {
            run(&[
                "btrfs",
                "subvolume",
                "snapshot",
                snap_root.join(tip.snap_name()).to_str().unwrap(),
                self.pool.live(name).to_str().unwrap(),
            ])?;
        }
        let fetched = missing.len();
        let layers = lineage.len();
        self.pool.set_lineage(name, &lineage).map_err(EngErr::other)?;
        Ok(PullOut { layers, fetched })
    }

    /// Materializes an explicit lineage (bypassing the registry entirely) — the seam `fsck`
    /// recovery uses when the registry's own records are what's lost: rebuild a lineage from
    /// object-store sidecars alone (`fsck::rebuild`), then restore straight from it.
    pub async fn pull_raw(&self, name: &str, lineage: Vec<LineageEntry>) -> Result<PullOut, EngErr> {
        self.pull_core(name, lineage).await
    }

    /// Materialize `ws`'s ref lineage locally, fetching only what's missing, then point live
    /// at the tip. Fails clean (no history yet) on a workspace that's never been pushed.
    pub async fn pull(&self, ws: &Workspace) -> Result<PullOut, EngErr> {
        let history = self.registry.get_history(&ws.owner, &ws.id).await.map_err(EngErr::other)?;
        let tip = history.first().ok_or_else(|| EngErr::other("workspace has no history; push first"))?;
        self.pull_core(&ws.id, tip.lineage.clone()).await
    }

    /// Env variant of `pull`: history keyed by `(owner, id)`, same "first = tip" convention.
    pub async fn pull_env(&self, owner: &str, id: &str) -> Result<PullOut, EngErr> {
        let history = self.registry.get_history(owner, id).await.map_err(EngErr::other)?;
        let tip = history.first().ok_or_else(|| EngErr::other("environment has no history; push first"))?;
        self.pull_core(id, tip.lineage.clone()).await
    }

    /// Reads `src_owner/src_id`'s current history from the registry and stages its tip lineage
    /// as `unpushed` under `dst_id` — the blobs already live in the object store (no upload
    /// needed), but `dst_id` has no `CommitRecord`s of its own until its next `push` writes
    /// them under `(dst_owner, dst_id)` and moves that volume's own ref. Per-entry `state`/
    /// `message`/`created_at` are recovered from the matching `CommitRecord` in `history` (an
    /// entry's own commit, keyed by blob id == commit id), falling back to the tip's own values
    /// for anything `history` doesn't explain (e.g. a block layer squashed after this history
    /// was fetched elsewhere — defensive, not expected on a linear push history).
    async fn inherit(&self, src_owner: &str, src_id: &str, dst_id: &str) -> Result<Vec<LineageEntry>, EngErr> {
        let history = self.registry.get_history(src_owner, src_id).await.map_err(EngErr::other)?;
        let tip = history.first().ok_or_else(|| EngErr::other("clone source has no snapshots; push first"))?.clone();
        let by_id: HashMap<&str, &CommitRecord> = history.iter().map(|r| (r.id.as_str(), r)).collect();

        let mut lineage = tip.lineage.clone();
        for e in lineage.iter_mut() {
            e.unpushed = true;
        }
        self.pool.set_lineage(dst_id, &lineage).map_err(EngErr::other)?;
        for e in &lineage {
            let (state, message, created_at) = match by_id.get(e.blob.as_str()) {
                Some(r) => (r.state.clone(), r.message.clone(), r.created_at),
                None => (tip.state.clone(), None, tip.created_at),
            };
            write_stage_meta(&self.pool, &e.blob, &StageMeta { raw: 0, clen: 0, state, message, created_at })?;
        }
        Ok(lineage)
    }

    /// Create `dst` from an EXACT commit id (not necessarily `src_owner/src_id`'s current tip)
    /// — backs `POST /v1/workspaces/restore`. Same staging-under-`dst` shape as
    /// `inherit`, just keyed to one named record out of the full history instead of `[0]`.
    pub async fn restore(
        &self,
        src_owner: &str,
        src_id: &str,
        commit_id: &str,
        dst_id: &str,
    ) -> Result<(), EngErr> {
        let history = self.registry.get_history(src_owner, src_id).await.map_err(EngErr::other)?;
        let record = history
            .iter()
            .find(|r| r.id == commit_id)
            .ok_or_else(|| EngErr::other("commit record not found"))?
            .clone();
        let by_id: HashMap<&str, &CommitRecord> = history.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut lineage = record.lineage.clone();
        for e in lineage.iter_mut() {
            e.unpushed = true;
        }
        self.pool.set_lineage(dst_id, &lineage).map_err(EngErr::other)?;
        for e in &lineage {
            let (state, message, created_at) = match by_id.get(e.blob.as_str()) {
                Some(r) => (r.state.clone(), r.message.clone(), r.created_at),
                None => (record.state.clone(), None, record.created_at),
            };
            write_stage_meta(&self.pool, &e.blob, &StageMeta { raw: 0, clen: 0, state, message, created_at })?;
        }
        self.pull_core(dst_id, lineage).await?;
        Ok(())
    }

    /// Local tip snapshot path for `id`, if `id` is fully materialized on THIS pool (voldir,
    /// lineage file, and the tip's actual snapshot directory all present) — the check `clone_local`
    /// uses to decide whether it can skip the registry entirely. A workspace that's only ever
    /// been committed, never pushed, still passes this: pushing is not a precondition for a
    /// same-pool clone, only for a cross-pool one.
    /// `None` lineage (never pushed even once — reachable if `WsCreate`'s push never landed, or
    /// in a test that seeds a live subvolume directly) still clones locally, straight off the
    /// live subvolume itself: there is no RO snapshot to point at, so `clone_local_snapshot`
    /// takes one of its own.
    fn local_tip(&self, id: &str) -> Option<std::path::PathBuf> {
        if !self.pool.voldir(id).exists() {
            return None;
        }
        let lineage = self.pool.lineage(id);
        match lineage.last() {
            Some(tip) => {
                let p = self.pool.snap_root(id).join(tip.snap_name());
                p.exists().then_some(p)
            }
            None => self.pool.live(id).exists().then(|| self.pool.live(id)),
        }
    }

    /// LOCAL-FIRST clone: `src` is materialized on this pool, so `dst` is built without a single
    /// registry call. The lineage file is copied to `dst` VERBATIM — `|u` unpushed marks
    /// included — so `dst` inherits exactly what `src` has, pushed and unpushed alike. Staged
    /// layer/meta files for any inherited unpushed entry are NOT copied: `Pool::stage_dir` is
    /// pool-global, keyed by blob id (`Pool::stage_path`), so `src` and `dst` already share the
    /// same files on disk — `dst`'s eventual `push` reads them straight off `src`'s staging.
    /// Two things guard that sharing: `spawn_janitor`'s stage sweep (`bins/agent/src/lib.rs`)
    /// unions unpushed blobs across every volume's lineage before deleting anything, so it
    /// already covers `dst` the instant this sets its lineage file; and `cleanup_local`'s
    /// `WsDelete` stage-file removal skips any blob still referenced by another volume's
    /// unpushed lineage, so deleting `src` after this clone can't strip a file `dst` still needs.
    /// Finally, a plain (RW) btrfs snapshot of `src`'s tip becomes `dst`'s live subvolume —
    /// same mechanics as `pull_core`'s tip restore, just sourced locally instead of from `recv/`.
    fn clone_local_snapshot(&self, src_id: &str, dst_id: &str) -> Result<(), EngErr> {
        let _lock = ws_lock(&self.pool, src_id).map_err(EngErr::other)?;
        let lineage = self.pool.lineage(src_id);
        // Never pushed (no lineage at all): snapshot the live subvolume directly rather than a
        // RO tip that doesn't exist yet — `dst` starts equally lineage-less.
        let tip_snap = match lineage.last() {
            Some(tip) => self.pool.snap_root(src_id).join(tip.snap_name()),
            None => self.pool.live(src_id),
        };
        drop(_lock);

        self.pool.set_lineage(dst_id, &lineage).map_err(EngErr::other)?;
        std::fs::create_dir_all(self.pool.voldir(dst_id)).map_err(EngErr::io)?;
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        // A replayed reconcile must converge, not fail: `dst` already existing means a previous
        // attempt got this far. Keep it — see `create_subvol`.
        if !self.pool.live(dst_id).exists() {
            run(&[
                "btrfs",
                "subvolume",
                "snapshot",
                tip_snap.to_str().unwrap(),
                self.pool.live(dst_id).to_str().unwrap(),
            ])?;
        }
        Ok(())
    }

    /// Clone `src` into `dst` (already created in `MetaStore`) for a stopped/never-pushed source
    /// — the agent picks this arm of `WsClone` when `src`'s container isn't running.
    /// LOCAL-FIRST: when `src` lives on this pool, `clone_local_snapshot` builds `dst` straight
    /// from local state — pushing is never a precondition when the source is on the same pool.
    /// Only when `src` isn't local here does this fall back to the registry-history path
    /// (`inherit` + `pull_core`), where `dst` still carries no registry history of its own until
    /// its next push, and "clone source has no snapshots; push first" is now reachable only
    /// cross-pool, where it's actually true.
    pub async fn clone_local(&self, src: &Workspace, dst: &Workspace) -> Result<(), EngErr> {
        self.clone_local_ids(&src.owner, &src.id, &dst.id).await
    }

    /// Id-only twin of `clone_local` — everything the local-first path needs (`local_tip`,
    /// `clone_local_snapshot`, `inherit`'s registry fallback) only ever reads an id/owner, never
    /// anything else off a `Workspace`/`Environment` doc, so this is what `clone_local` calls and
    /// what an environment clone (a different doc type, same volume-id shape to the engine) calls
    /// directly.
    pub async fn clone_local_ids(&self, src_owner: &str, src_id: &str, dst_id: &str) -> Result<(), EngErr> {
        if self.local_tip(src_id).is_some() {
            return self.clone_local_snapshot(src_id, dst_id);
        }
        let lineage = self.inherit(src_owner, src_id, dst_id).await?;
        self.pull_core(dst_id, lineage).await?;
        Ok(())
    }

    /// Clone a RUNNING workspace, minimizing source downtime. Routes on whether `src`'s live
    /// subvolume is materialized on THIS pool — the same locality signal `local_tip`/
    /// `clone_local` use — not on the registry: a running container only ever runs against a
    /// LOCAL subvolume, so this is the common (in practice, the only reachable-today) case.
    /// `clone_running_registry` is kept for a genuine cross-node running clone, a shape the
    /// owner-binding scheduler doesn't produce yet but the engine shouldn't assume never will.
    pub async fn clone_running(
        &self,
        src: &Workspace,
        dst: &Workspace,
        stop: &dyn Fn() -> Result<(), EngErr>,
        start: &dyn Fn() -> Result<(), EngErr>,
    ) -> Result<CloneOut, EngErr> {
        if self.pool.voldir(&src.id).exists() {
            self.clone_running_local(&src.id, &dst.id, stop, start).await
        } else {
            self.clone_running_registry(src, dst, stop, start).await
        }
    }

    /// Local-first running clone: `src` lives on this pool, so this never touches the registry
    /// and never requires `src` to have pushed (or even snapshotted) at all — a never-pushed
    /// running workspace used to fail here with "clone source has no snapshots; push first"
    /// because the registry-prefetch path below was the only one `clone_running` had. Stop,
    /// flush, plain (RW) btrfs-snapshot `src`'s LIVE subvolume straight into `dst`'s live —
    /// same "snapshot the live subvolume, not an RO tip" trick `clone_local_snapshot` uses for a
    /// never-pushed STOPPED source, just done live instead of on an already-quiesced volume —
    /// then copy `src`'s lineage file to `dst` VERBATIM (marks included, possibly empty: `dst`
    /// simply starts with no history until its own first push, same as any local-first clone).
    /// Id-only (like `clone_local_ids`): a running container's `stop`/`start` hooks are already
    /// exact-name shell-outs the caller builds, so nothing here needs a typed doc either — an
    /// environment clone calls this directly with its own (compose-project) hooks.
    pub async fn clone_running_local(
        &self,
        src_id: &str,
        dst_id: &str,
        stop: &dyn Fn() -> Result<(), EngErr>,
        start: &dyn Fn() -> Result<(), EngErr>,
    ) -> Result<CloneOut, EngErr> {
        let t0 = Instant::now();
        stop()?;
        let synced = run(&["sync", "-f", self.pool.live(src_id).to_str().unwrap()]);
        let snapshotted = (|| -> Result<(), EngErr> {
            synced?;
            let _lock = ws_lock(&self.pool, src_id).map_err(EngErr::other)?;
            std::fs::create_dir_all(self.pool.voldir(dst_id)).map_err(EngErr::io)?;
            // Idempotent replay, as in `create_subvol`: the source is stopped for this window, so
            // a `dst` left by a previous attempt holds the same bytes this one would take.
            if !self.pool.live(dst_id).exists() {
                run(&[
                    "btrfs",
                    "subvolume",
                    "snapshot",
                    self.pool.live(src_id).to_str().unwrap(),
                    self.pool.live(dst_id).to_str().unwrap(),
                ])?;
            }
            self.pool.set_lineage(dst_id, &self.pool.lineage(src_id)).map_err(EngErr::other)?;
            Ok(())
        })();
        // `start` must run even if the snapshot failed, so the source is never left stopped —
        // same contract the registry path below has.
        let started = start();
        if let Err(e) = snapshotted {
            return Err(match started {
                Ok(()) => e,
                Err(se) => EngErr::other(format!("{e}; additionally start failed: {se}")),
            });
        }
        started?;
        let locked = t0.elapsed();
        Ok(CloneOut { prefetched: Duration::ZERO, locked, total: t0.elapsed() })
    }

    /// Cross-node running clone: `src` is NOT materialized on this pool, so the only way to copy
    /// it is through the registry — requires `src` to have pushed at least once (`inherit`'s own
    /// "clone source has no snapshots; push first"), unlike the local path above.
    ///
    /// Phase 1 (source untouched): prefetch — pull everything up to the source's last pushed
    /// snapshot, so the bulk transfer happens while the source keeps running. Phase 2 (container
    /// lock): `stop`, sync, push the final delta (small by construction — the prefetch absorbed
    /// the rest), then `start` as soon as that delta is durable. Phase 3: re-stage the clone from
    /// the now-current source history and fetch just that last delta.
    async fn clone_running_registry(
        &self,
        src: &Workspace,
        dst: &Workspace,
        stop: &dyn Fn() -> Result<(), EngErr>,
        start: &dyn Fn() -> Result<(), EngErr>,
    ) -> Result<CloneOut, EngErr> {
        let t0 = Instant::now();

        // Phase 1: warm this pool up to the last pushed commit; source keeps running.
        let lineage1 = self.inherit(&src.owner, &src.id, &dst.id).await?;
        self.pull_core(&dst.id, lineage1).await?;
        let prefetched = t0.elapsed();

        // Phase 2: the locked window — only the final delta happens inside it. `start` must
        // run even if the push fails, so the source is never left stopped; on that path the
        // error still propagates (with start's error appended if it also failed).
        let t1 = Instant::now();
        stop()?;
        let synced = run(&["sync", "-f", self.pool.live(&src.id).to_str().unwrap()]);
        let pushed = match synced {
            Ok(()) => self.push(src, None).await.map(|_| ()),
            Err(e) => Err(e),
        };
        let started = start();
        if let Err(e) = pushed {
            return Err(match started {
                Ok(()) => e,
                Err(se) => EngErr::other(format!("{e}; additionally start failed: {se}")),
            });
        }
        started?;
        let locked = t1.elapsed();

        // Phase 3: re-stage the clone from the source's now-current history and apply the one
        // missing delta.
        let lineage3 = self.inherit(&src.owner, &src.id, &dst.id).await?;
        if self.pool.live(&dst.id).exists() {
            run(&["btrfs", "subvolume", "delete", self.pool.live(&dst.id).to_str().unwrap()])?;
        }
        self.pull_core(&dst.id, lineage3).await?;

        Ok(CloneOut { prefetched, locked, total: t0.elapsed() })
    }

    /// Convert the local tip into a block layer: a mountable btrfs image holding the tip
    /// snapshot, populated by a LOCAL send/receive (the per-file cost paid here, once, in the
    /// background, never again on restore). The new lineage is that single block entry plus
    /// any streams grafted on after a racing commit landed while the image was building. The
    /// block blob is uploaded directly here (not staged — squash already streams straight to
    /// the object store, same as before the commit/push split), so `push`'s "no staged file ⇒
    /// registration-only" path picks it up without a redundant upload. Called by the detached
    /// `rustic-git-agent squash <ws-id>` child spawned from `push`.
    pub async fn squash(&self, ws: &Workspace) -> Result<(), EngErr> {
        let latch = self.pool.root.join("vol").join(format!("{}.squashing", ws.id));
        let r = self.squash_inner(ws).await;
        let _ = std::fs::remove_file(&latch);
        r
    }

    async fn squash_inner(&self, ws: &Workspace) -> Result<(), EngErr> {
        let lineage = self.pool.lineage(&ws.id);
        let tip = lineage.last().ok_or_else(|| EngErr::other("no lineage; push first"))?.snap_name().to_string();
        let root = self.pool.snap_root(&ws.id);

        // Size the image from the tip's content plus btrfs overhead headroom.
        let du = std::process::Command::new("du")
            .args(["-sb", root.join(&tip).to_str().unwrap()])
            .output()
            .map_err(EngErr::io)?;
        let used: u64 = String::from_utf8_lossy(&du.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| EngErr::other("du failed"))?;
        // Headroom: btrfs metadata for many small files is far from free, so 50% + 1G slack;
        // the image is sparse and zstd flattens the unused tail to nearly nothing.
        let size = used + used / 2 + (1 << 30);

        let blob_id = uuid();
        std::fs::create_dir_all(self.pool.img_dir()).map_err(EngErr::io)?;
        let img = self.pool.img(&blob_id);
        run(&["truncate", "-s", &size.to_string(), img.to_str().unwrap()])?;
        run(&["mkfs.btrfs", "-q", "-m", "single", "-d", "single", img.to_str().unwrap()])?;
        let mnt = format!("/tmp/wssquash-{blob_id}");
        std::fs::create_dir_all(&mnt).map_err(EngErr::io)?;
        run(&["mount", "-o", "loop", img.to_str().unwrap(), &mnt])?;
        let populate = (|| -> Result<(), EngErr> {
            let mut send = std::process::Command::new("btrfs")
                .args(["send", "-q", root.join(&tip).to_str().unwrap()])
                .stdout(Stdio::piped())
                .spawn()
                .map_err(EngErr::io)?;
            let mut recv = std::process::Command::new("btrfs")
                .args(["receive", "-q", &mnt])
                .stdin(send.stdout.take().unwrap())
                .spawn()
                .map_err(EngErr::io)?;
            if !recv.wait().map_err(EngErr::io)?.success() || !send.wait().map_err(EngErr::io)?.success() {
                return Err(EngErr::other("populate send/receive failed"));
            }
            Ok(())
        })();
        // umount can fail under load (a lingering child still holding the mount). Retry lazily
        // rather than returning early: an un-umounted /tmp/wssquash-* pins the loop device and the
        // image file forever, and a squash failure isn't worth leaking a mount over.
        let umounted = run(&["umount", &mnt]).or_else(|e| run(&["umount", "-l", &mnt]).map_err(|_| e));
        let _ = std::fs::remove_dir(&mnt);
        populate?;
        umounted?;

        let f = std::fs::File::open(&img).map_err(EngErr::io)?;
        let (raw, clen, sha) =
            blob::upload_stream(self.store.as_ref(), &format!("layers/{blob_id}.zst"), f).await.map_err(EngErr::other)?;
        // The build image has served its only purpose: its bytes are durable in the object store,
        // and a restore re-fetches them into a fresh `{pool}/img/{blob}.img`. Keeping it grew the
        // pool by one full workspace image per squash, forever.
        let _ = std::fs::remove_file(&img);
        let parent_blob = lineage.last().map(|e| e.blob.clone());
        blob::write_sidecar(
            self.store.as_ref(),
            &blob_id,
            &blob::LayerSidecar {
                kind: LayerKind::Block,
                parent_blob,
                snap_uuid: Some(tip.clone()),
                sha256: sha.clone(),
                raw,
                stored: clen,
                created_at: chrono::Utc::now(),
            },
        )
        .await
        .map_err(EngErr::other)?;

        // The squash races commits that landed while the image was building: under the lock,
        // re-read the local lineage and graft any streams that arrived after our tip onto the
        // new block base, so their history is preserved rather than clobbered. Every grafted
        // stream is guaranteed still `unpushed` here — nothing pushes without draining every
        // unpushed entry first, so a commit made after squash started can't have been pushed
        // yet by anything else.
        let _lock = ws_lock(&self.pool, &ws.id).map_err(EngErr::other)?;
        let now = self.pool.lineage(&ws.id);
        let mut new_lineage = vec![LineageEntry {
            kind: LayerKind::Block,
            blob: blob_id.clone(),
            snap: Some(tip.clone()),
            sha256: sha.clone(),
            unpushed: true,
        }];
        let after: Vec<LineageEntry> = now.iter().skip_while(|e| e.snap_name() != tip).skip(1).cloned().collect();
        new_lineage.extend(after);
        self.pool.set_lineage(&ws.id, &new_lineage).map_err(EngErr::other)?;
        write_stage_meta(
            &self.pool,
            &blob_id,
            &StageMeta { raw, clen, state: ws.live_state.clone(), message: Some("auto-squash".into()), created_at: chrono::Utc::now() },
        )?;
        drop(_lock);

        // Not the fused `push`: the block entry above is already staged directly (no fresh
        // `commit_core` snapshot wanted on top of it), so this goes straight to the upload phase.
        self.upload_core(&ws.owner, &ws.id).await?;
        Ok(())
    }
}
