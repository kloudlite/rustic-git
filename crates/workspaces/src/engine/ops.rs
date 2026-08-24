//! Engine operations: push (with auto-squash), pull, fork, clone_running, squash. Ported from
//! `docs/superpowers/poc/wssnap/main.rs` (Azure-tested); the only intended differences are refs
//! and lineage records moving from S3 objects (`refs/{ws}`, `snaps/{uuid}.json`) to
//! `MetaStore`: a push writes a `Snapshot` doc then moves `Workspace.ref_` by etag CAS.
//! `remote_lineage` becomes: get the workspace doc -> `get_snapshot(ws.id, ref)` -> lineage.
//!
//! Because `Snapshot` docs are partitioned by owning workspace id, fork/clone don't just copy
//! a ref value — they duplicate the snapshot doc under the destination workspace's id too (see
//! `copy_ref`), so `remote_lineage` resolves the same way for every workspace.

use crate::engine::{Pool, blob, is_mountpoint, ws_lock};
use crate::model::{LayerKind, LineageEntry, Snapshot, Workspace};
use crate::store::{MetaStore, StoreErr};
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as S3Path};
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
    pub(crate) fn store(e: StoreErr) -> Self {
        EngErr(format!("{e:?}"))
    }
    fn io(e: std::io::Error) -> Self {
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

pub struct Engine {
    pub pool: Pool,
    pub store: Arc<dyn ObjectStore>,
    pub meta: Arc<dyn MetaStore>,
    /// Delta size (MB) that forces a block layer. Env `WSSNAP_SQUASH_MB`, default 256.
    pub squash_mb: u64,
    /// Stream layers since the last block layer that force one. Env `WSSNAP_CHAIN_MAX`, default 50.
    pub chain_max: usize,
}

impl Engine {
    pub fn new(pool: Pool, store: Arc<dyn ObjectStore>, meta: Arc<dyn MetaStore>) -> Engine {
        Engine {
            pool,
            store,
            meta,
            squash_mb: std::env::var("WSSNAP_SQUASH_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(256),
            chain_max: std::env::var("WSSNAP_CHAIN_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(50),
        }
    }

    /// Move `owner/id`'s ref to `r` (and, when given, its `live_state`) under etag CAS: read,
    /// attempt, and on `CasFailed` re-read and retry exactly once before giving up — the
    /// double-squash bug came from skipping this.
    async fn set_ref(
        &self,
        owner: &str,
        id: &str,
        r: &str,
        state: Option<&serde_json::Value>,
    ) -> Result<Workspace, EngErr> {
        let mut retried = false;
        loop {
            let (mut w, etag) = self
                .meta
                .get_ws(owner, id)
                .await
                .map_err(EngErr::store)?
                .ok_or_else(|| EngErr::other(format!("workspace {owner}/{id} not found")))?;
            w.ref_ = Some(r.to_string());
            if let Some(s) = state {
                w.live_state = s.clone();
            }
            match self.meta.replace_ws(&w, &etag).await {
                Ok(()) => return Ok(w),
                Err(StoreErr::CasFailed) if !retried => {
                    retried = true;
                    continue;
                }
                Err(e) => return Err(EngErr::store(e)),
            }
        }
    }

    /// Write a snapshot record for `lineage` (capturing `ws`'s current `live_state`) and point
    /// `ws`'s ref at it.
    async fn commit(&self, ws: &Workspace, lineage: &[LineageEntry]) -> Result<String, EngErr> {
        let snap_id = uuid();
        let snap = Snapshot {
            id: snap_id.clone(),
            workspace_id: ws.id.clone(),
            lineage: lineage.to_vec(),
            created_at: chrono::Utc::now(),
            state: ws.live_state.clone(),
        };
        self.meta.put_snapshot(&snap).await.map_err(EngErr::store)?;
        self.set_ref(&ws.owner, &ws.id, &snap_id, None).await?;
        Ok(snap_id)
    }

    /// Ref -> snapshot record -> ordered entries.
    async fn remote_lineage(&self, owner: &str, id: &str) -> Result<(Workspace, Vec<LineageEntry>), EngErr> {
        let (w, _etag) = self
            .meta
            .get_ws(owner, id)
            .await
            .map_err(EngErr::store)?
            .ok_or_else(|| EngErr::other(format!("workspace {owner}/{id} not found")))?;
        let r = w.ref_.clone().ok_or_else(|| EngErr::other("workspace has no ref; push first"))?;
        let snap = self
            .meta
            .get_snapshot(&w.id, &r)
            .await
            .map_err(EngErr::store)?
            .ok_or_else(|| EngErr::other("snapshot record missing"))?;
        Ok((w, snap.lineage))
    }

    /// Duplicate `src`'s current snapshot doc under `dst`'s id and point `dst`'s ref + inherited
    /// `live_state` at it — `Snapshot` docs are partitioned by owning workspace, so a bare ref
    /// copy wouldn't resolve, and the destination's state comes from the snapshot being
    /// forked/cloned, not from the live source doc (which may have moved on since).
    async fn copy_ref(&self, src: &Workspace, dst: &Workspace) -> Result<(), EngErr> {
        let (src_ws, _) = self
            .meta
            .get_ws(&src.owner, &src.id)
            .await
            .map_err(EngErr::store)?
            .ok_or_else(|| EngErr::other("src workspace not found"))?;
        let r = src_ws.ref_.clone().ok_or_else(|| EngErr::other("src has no ref; push first"))?;
        let snap = self
            .meta
            .get_snapshot(&src_ws.id, &r)
            .await
            .map_err(EngErr::store)?
            .ok_or_else(|| EngErr::other("src snapshot record missing"))?;
        self.graft_snapshot(dst, snap, &r).await
    }

    /// Duplicate `snap` (already read from wherever it lives) under `dst`'s id, keyed by `r`,
    /// and move `dst`'s ref + `live_state` onto it. Shared by `copy_ref` and
    /// `create_from_snapshot`.
    async fn graft_snapshot(&self, dst: &Workspace, snap: Snapshot, r: &str) -> Result<(), EngErr> {
        let state = snap.state.clone();
        let mut dst_snap = snap;
        dst_snap.workspace_id = dst.id.clone();
        self.meta.put_snapshot(&dst_snap).await.map_err(EngErr::store)?;
        self.set_ref(&dst.owner, &dst.id, r, Some(&state)).await?;
        Ok(())
    }

    /// Create `dst` from an EXACT snapshot record (not necessarily `src_ws_id`'s current ref
    /// tip) — backs `POST /v1/workspaces/from-snapshot`. Duplicates that record under `dst`'s
    /// id, inherits its `live_state`, and materializes it locally.
    pub async fn create_from_snapshot(
        &self,
        src_ws_id: &str,
        snapshot_id: &str,
        dst: &Workspace,
    ) -> Result<(), EngErr> {
        let snap = self
            .meta
            .get_snapshot(src_ws_id, snapshot_id)
            .await
            .map_err(EngErr::store)?
            .ok_or_else(|| EngErr::other("snapshot record not found"))?;
        self.graft_snapshot(dst, snap, snapshot_id).await?;
        self.pull(dst).await?;
        Ok(())
    }

    pub async fn init(&self, ws: &Workspace) -> Result<(), EngErr> {
        std::fs::create_dir_all(self.pool.wsdir(&ws.id)).map_err(EngErr::io)?;
        run(&["btrfs", "subvolume", "create", self.pool.live(&ws.id).to_str().unwrap()])?;
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        self.push(ws).await?;
        Ok(())
    }

    /// Snapshot live, upload the delta, extend the lineage, move the ref. Auto-squash: the
    /// push itself stays fast (the delta is already durable); the block layer is built by a
    /// detached `rustic-git-agent squash <ws-id>` child so this returns immediately.
    pub async fn push(&self, ws: &Workspace) -> Result<PushOut, EngErr> {
        let _lock = ws_lock(&self.pool, &ws.id).map_err(EngErr::other)?;
        let mut lineage = self.pool.lineage(&ws.id);
        let root = self.pool.snap_root(&ws.id);
        let parent = lineage.last().map(|e| root.join(e.snap_name()));
        let t = Instant::now();
        let id = uuid();
        run(&[
            "btrfs",
            "subvolume",
            "snapshot",
            "-r",
            self.pool.live(&ws.id).to_str().unwrap(),
            root.join(&id).to_str().unwrap(),
        ])?;
        let mut child = blob::spawn_send(&root.join(&id), parent).map_err(EngErr::other)?;
        let (raw, clen, sha) =
            blob::upload_stream(self.store.as_ref(), &format!("layers/{id}.zst"), child.stdout.take().unwrap())
                .await
                .map_err(EngErr::other)?;
        let st = child.wait_with_output().map_err(EngErr::io)?;
        if !st.status.success() {
            return Err(EngErr::other(format!("btrfs send: {}", String::from_utf8_lossy(&st.stderr))));
        }
        let parent_blob = lineage.last().map(|e| e.blob.clone());
        blob::write_sidecar(
            self.store.as_ref(),
            &id,
            &blob::LayerSidecar {
                kind: LayerKind::Stream,
                parent_blob,
                snap_uuid: None,
                sha256: sha.clone(),
                raw,
                stored: clen,
                created_at: chrono::Utc::now(),
            },
        )
        .await
        .map_err(EngErr::other)?;
        lineage.push(LineageEntry { kind: LayerKind::Stream, blob: id.clone(), snap: None, sha256: sha.clone() });
        self.commit(ws, &lineage).await?;
        self.pool.set_lineage(&ws.id, &lineage);

        let since_block = lineage.iter().rev().take_while(|e| e.kind == LayerKind::Stream).count();
        let reason = if raw > self.squash_mb << 20 {
            Some(format!("delta > {}MB", self.squash_mb))
        } else if since_block > self.chain_max {
            Some(format!("chain > {}", self.chain_max))
        } else {
            None
        };

        // The latch stops a second squash from spawning while one is still building; the
        // squash child removes it when done.
        let latch = self.pool.root.join("ws").join(format!("{}.squashing", ws.id));
        let mut squash_triggered = None;
        if let Some(r) = reason {
            if latch.exists() {
                squash_triggered = Some(format!("{r} (already running)"));
            } else {
                std::fs::write(&latch, b"").map_err(EngErr::io)?;
                let exe = std::env::current_exe().map_err(EngErr::io)?;
                std::process::Command::new(exe)
                    .args(["squash", &ws.id])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(EngErr::io)?;
                squash_triggered = Some(r);
            }
        }

        Ok(PushOut {
            layer: id,
            sha,
            raw,
            compressed: clen,
            layers: lineage.len(),
            squash_triggered,
            elapsed: t.elapsed(),
        })
    }

    /// Materialize `ws`'s ref lineage locally, fetching only what's missing, then point live
    /// at the tip. A lineage whose base is a block layer not yet local restores by image
    /// mount: decompress straight to a loop-mounted fs, no per-file receive for the bulk.
    pub async fn pull(&self, ws: &Workspace) -> Result<PullOut, EngErr> {
        let (ws_doc, lineage) = self.remote_lineage(&ws.owner, &ws.id).await?;
        let name = ws_doc.id.as_str();
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        std::fs::create_dir_all(self.pool.wsdir(name)).map_err(EngErr::io)?;

        // Block fast path only when the base isn't already materialized on the shared pool.
        let mut snap_root = self.pool.recv();
        let mut rest = &lineage[..];
        if let Some(first) = lineage.first() {
            if first.kind == LayerKind::Block {
                let snap_name = first.snap_name();
                if !self.pool.recv().join(snap_name).exists() {
                    let wsroot = self.pool.wsdir(name);
                    if !is_mountpoint(&wsroot) {
                        // Stream download -> decode -> disk; nothing buffers the whole image.
                        let mut s = self
                            .store
                            .get(&S3Path::from(format!("layers/{}.zst", first.blob)))
                            .await
                            .map_err(|e| EngErr::other(e.to_string()))?
                            .into_stream();
                        std::fs::create_dir_all(self.pool.root.join("img")).map_err(EngErr::io)?;
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
        self.pool.set_lineage(name, &lineage);
        Ok(PullOut { layers, fetched })
    }

    /// Fork `src`'s current snapshot into `dst` (already created in `MetaStore`) and
    /// materialize it locally — no source downtime, no re-upload of shared ancestors.
    pub async fn fork(&self, src: &Workspace, dst: &Workspace) -> Result<(), EngErr> {
        self.copy_ref(src, dst).await?;
        self.pull(dst).await?;
        Ok(())
    }

    /// Clone a RUNNING workspace onto this engine's pool, minimizing source downtime.
    ///
    /// Phase 1 (source untouched): prefetch — pull everything up to the source's last saved
    /// snapshot, so the bulk transfer happens while the source keeps running. Phase 2
    /// (container lock): `stop`, sync, push the final delta (small by construction — the
    /// prefetch absorbed the rest), then `start` as soon as that delta is durable. Phase 3:
    /// point the clone's ref at the frozen snapshot and fetch just that last delta.
    pub async fn clone_running(
        &self,
        src: &Workspace,
        dst: &Workspace,
        stop: &dyn Fn() -> Result<(), EngErr>,
        start: &dyn Fn() -> Result<(), EngErr>,
    ) -> Result<CloneOut, EngErr> {
        let t0 = Instant::now();

        // Phase 1: warm this pool up to the last saved snapshot; source keeps running.
        self.copy_ref(src, dst).await?;
        self.pull(dst).await?;
        let prefetched = t0.elapsed();

        // Phase 2: the locked window — only the final delta happens inside it. `start` must
        // run even if sync/push fails, so the source is never left stopped; on that path the
        // sync/push error still propagates (with start's error appended if it also failed).
        let t1 = Instant::now();
        stop()?;
        let synced = run(&["sync", "-f", self.pool.live(&src.id).to_str().unwrap()]);
        let pushed = match synced {
            Ok(()) => self.push(src).await.map(|_| ()),
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

        // Phase 3: re-point the clone at the frozen snapshot and apply the one missing delta.
        self.copy_ref(src, dst).await?;
        if self.pool.live(&dst.id).exists() {
            run(&["btrfs", "subvolume", "delete", self.pool.live(&dst.id).to_str().unwrap()])?;
        }
        self.pull(dst).await?;

        Ok(CloneOut { prefetched, locked, total: t0.elapsed() })
    }

    /// Convert the local tip into a block layer: a mountable btrfs image holding the tip
    /// snapshot, populated by a LOCAL send/receive (the per-file cost paid here, once, in the
    /// background, never again on restore). The new lineage is that single block entry plus
    /// any streams grafted on after a racing push landed while the image was building. Called
    /// by the detached `rustic-git-agent squash <ws-id>` child spawned from `push`.
    pub async fn squash(&self, ws: &Workspace) -> Result<(), EngErr> {
        let latch = self.pool.root.join("ws").join(format!("{}.squashing", ws.id));
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
        std::fs::create_dir_all(self.pool.root.join("img")).map_err(EngErr::io)?;
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
        run(&["umount", &mnt])?;
        let _ = std::fs::remove_dir(&mnt);
        populate?;

        let f = std::fs::File::open(&img).map_err(EngErr::io)?;
        let (raw, clen, sha) =
            blob::upload_stream(self.store.as_ref(), &format!("layers/{blob_id}.zst"), f).await.map_err(EngErr::other)?;
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

        // The commit races pushes that landed while the image was building: under the lock,
        // re-read the local lineage and graft any streams that arrived after our tip onto the
        // new block base, so their history is preserved rather than clobbered.
        let _lock = ws_lock(&self.pool, &ws.id).map_err(EngErr::other)?;
        let now = self.pool.lineage(&ws.id);
        let mut new_lineage =
            vec![LineageEntry { kind: LayerKind::Block, blob: blob_id.clone(), snap: Some(tip.clone()), sha256: sha }];
        let after: Vec<LineageEntry> = now.iter().skip_while(|e| e.snap_name() != tip).skip(1).cloned().collect();
        new_lineage.extend(after);
        self.commit(ws, &new_lineage).await?;
        self.pool.set_lineage(&ws.id, &new_lineage);
        Ok(())
    }
}
