//! wssnap POC: workspace = btrfs subvolume, snapshot = RO snapshot, delta = incremental
//! `btrfs send -p` stream, zstd-compressed, stored in S3 (MinIO).
//!
//! Lineage model: every layer blob is immutable, named by a UUID; a snapshot record stores
//! the FULL ordered list of layer entries from the base up to itself, so records are freely
//! deletable and forks share ancestors' blobs. Refs name a snapshot record.
//! Lineage entries are "s:{blob}" (send stream) or "b:{blob}:{snap}" (block image whose
//! contained subvolume is named {snap} — the stream snapshot it materializes, so streams
//! chain across the block boundary by received-UUID exactly as they would over the wire).
//!
//! S3 layout:
//!   layers/{uuid}.zst      zstd send stream or zstd block image
//!   snaps/{uuid}.json      {"lineage": ["s:...", "b:...:...", ...]}
//!   refs/{ws}              snapshot record uuid
//!
//! Pool layout:
//!   {pool}/ws/{name}/live    RW subvolume; for a block-restored workspace, {pool}/ws/{name}
//!                            is a loop mount of the image — its own filesystem.
//!   {pool}/ws/{name}.lineage local ordered entry list for live (outside the mount on purpose)
//!   {pool}/recv/{snap}       RO snapshots on the shared pool fs — the local layer cache.
//!   {pool}/img/{blob}.img    decompressed block images backing mounted workspaces.
//!
//! Run as root: btrfs subvolume/send/receive/mount need it.

use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path as S3Path};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

fn uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap().trim().into()
}

#[derive(Clone)]
enum Entry {
    Stream { blob: String, sha: String },
    Block { blob: String, snap: String, sha: String },
}

impl Entry {
    fn parse(s: &str) -> Entry {
        let p: Vec<&str> = s.split(':').collect();
        match p[0] {
            "b" => Entry::Block { blob: p[1].into(), snap: p[2].into(), sha: p[3].into() },
            _ => Entry::Stream { blob: p[1].into(), sha: p[2].into() },
        }
    }
    fn encode(&self) -> String {
        match self {
            Entry::Stream { blob, sha } => format!("s:{blob}:{sha}"),
            Entry::Block { blob, snap, sha } => format!("b:{blob}:{snap}:{sha}"),
        }
    }
    fn blob(&self) -> &str {
        match self {
            Entry::Stream { blob, .. } | Entry::Block { blob, .. } => blob,
        }
    }
    fn sha(&self) -> &str {
        match self {
            Entry::Stream { sha, .. } | Entry::Block { sha, .. } => sha,
        }
    }
    /// Name of the local RO snapshot this entry materializes.
    fn snap(&self) -> &str {
        match self {
            Entry::Stream { blob, .. } => blob,
            Entry::Block { snap, .. } => snap,
        }
    }
}

fn sha_hex(h: sha2::Sha256) -> String {
    use sha2::Digest;
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Serialize every lineage read-modify-write for one workspace across processes (push vs the
/// background squash) — the double-squash came from exactly this race.
fn ws_lock(pool: &Pool, ws: &str) -> Result<std::fs::File, String> {
    let path = pool.root.join("ws").join(format!("{ws}.lock"));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("flock failed".into());
    }
    Ok(f)
}

struct Pool {
    root: PathBuf,
}

impl Pool {
    fn new(root: &str) -> Pool {
        Pool { root: root.into() }
    }
    fn recv(&self) -> PathBuf {
        self.root.join("recv")
    }
    fn img(&self, blob: &str) -> PathBuf {
        self.root.join("img").join(format!("{blob}.img"))
    }
    fn wsdir(&self, name: &str) -> PathBuf {
        self.root.join("ws").join(name)
    }
    fn live(&self, name: &str) -> PathBuf {
        self.wsdir(name).join("live")
    }
    /// Where this workspace's snapshots live: inside the image mount for a block-restored
    /// workspace (its own fs — snapshots cannot cross filesystems), else the shared recv/.
    fn snap_root(&self, name: &str) -> PathBuf {
        if is_mountpoint(&self.wsdir(name)) { self.wsdir(name) } else { self.recv() }
    }
    fn lineage(&self, name: &str) -> Vec<Entry> {
        std::fs::read_to_string(self.root.join("ws").join(format!("{name}.lineage")))
            .map(|s| s.lines().map(Entry::parse).collect())
            .unwrap_or_default()
    }
    fn set_lineage(&self, name: &str, l: &[Entry]) {
        let s: Vec<String> = l.iter().map(Entry::encode).collect();
        std::fs::write(self.root.join("ws").join(format!("{name}.lineage")), s.join("\n")).unwrap();
    }
}

fn is_mountpoint(p: &std::path::Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mounts.lines().any(|l| l.split_whitespace().nth(1) == p.to_str())
}

fn s3() -> Arc<dyn ObjectStore> {
    // Azure Blob when AZURE_ACCOUNT is set; the local MinIO otherwise.
    if let Ok(account) = std::env::var("AZURE_ACCOUNT") {
        return Arc::new(
            object_store::azure::MicrosoftAzureBuilder::new()
                .with_account(account)
                .with_access_key(std::env::var("AZURE_KEY").expect("AZURE_KEY"))
                .with_container_name(std::env::var("AZURE_CONTAINER").expect("AZURE_CONTAINER"))
                .build()
                .unwrap(),
        );
    }
    Arc::new(
        object_store::aws::AmazonS3Builder::new()
            .with_endpoint(std::env::var("S3_URL").unwrap_or("http://127.0.0.1:9000".into()))
            .with_bucket_name("wslayers")
            .with_access_key_id("admin")
            .with_secret_access_key("adminadmin")
            .with_allow_http(true)
            .with_region("us-east-1")
            .build()
            .unwrap(),
    )
}

fn run(argv: &[&str]) -> Result<(), String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    if !out.status.success() {
        return Err(format!("{argv:?}: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// Spawn `btrfs send` for the snapshot at `path` (incremental against `parent` when given),
/// handing back the child so its stdout can stream straight into the uploader.
fn spawn_send(path: &PathBuf, parent: Option<PathBuf>) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("btrfs");
    cmd.args(["send", "-q"]);
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    cmd.arg(path);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())
}

/// Decode a layer blob (leading mode byte, then zstd or raw) into `btrfs receive` at `dir`.
fn receive_into(dir: &PathBuf, blob: &[u8]) -> Result<(), String> {
    let (mode, comp) = blob.split_first().ok_or("empty blob")?;
    let mut child = Command::new("btrfs")
        .args(["receive", "-q", dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().unwrap();
    if *mode == b'z' {
        let mut dec = zstd::Decoder::new(comp).map_err(|e| e.to_string())?;
        std::io::copy(&mut dec, &mut stdin).map_err(|e| e.to_string())?;
    } else {
        stdin.write_all(comp).map_err(|e| e.to_string())?;
    }
    drop(stdin);
    let st = child.wait_with_output().map_err(|e| e.to_string())?;
    if !st.status.success() {
        return Err(format!("btrfs receive: {}", String::from_utf8_lossy(&st.stderr)));
    }
    Ok(())
}

async fn get_bytes(store: &dyn ObjectStore, key: &str) -> Result<Vec<u8>, String> {
    Ok(store
        .get(&S3Path::from(key))
        .await
        .map_err(|e| format!("{key}: {e}"))?
        .bytes()
        .await
        .map_err(|e| e.to_string())?
        .to_vec())
}

async fn put_bytes(store: &dyn ObjectStore, key: &str, b: Vec<u8>) -> Result<(), String> {
    store.put(&S3Path::from(key), PutPayload::from(b)).await.map_err(|e| e.to_string())?;
    Ok(())
}

const CHUNK: usize = 32 << 20;

/// Sends chunks produced by a compressing thread into the async uploader.
struct ChanWriter {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    buf: Vec<u8>,
}

impl Write for ChanWriter {
    fn write(&mut self, d: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(d);
        if self.buf.len() >= CHUNK {
            let full = std::mem::take(&mut self.buf);
            self.tx.blocking_send(full).map_err(|_| std::io::Error::other("upload gone"))?;
        }
        Ok(d.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Streaming layer upload: reader -> (zstd | raw) -> multipart, all three overlapped, so
/// wall time is max(produce, network) instead of their sum. Multipart parts retry
/// independently — a single giant PUT dies to the retry timeout on a slow uplink.
/// The blob's first byte says how the rest is encoded: 'z' zstd, 'r' raw — chosen by
/// test-compressing the first chunk, so incompressible payloads skip zstd entirely.
async fn upload_stream(
    store: &dyn ObjectStore,
    key: &str,
    mut r: impl Read + Send + 'static,
) -> Result<(u64, u64, String), String> {
    // Read the first chunk and decide the mode from what zstd does to it.
    let mut first = vec![0u8; CHUNK];
    let mut n = 0;
    while n < CHUNK {
        let k = r.read(&mut first[n..]).map_err(|e| e.to_string())?;
        if k == 0 {
            break;
        }
        n += k;
    }
    first.truncate(n);
    let compressible = zstd::bulk::compress(&first[..first.len().min(4 << 20)], 1)
        .map(|c| (c.len() as f64) < 0.97 * (first.len().min(4 << 20).max(1) as f64))
        .unwrap_or(true);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let raw_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rc = raw_count.clone();
    let producer = std::thread::spawn(move || -> Result<(), String> {
        struct Counted<R: Read>(R, Arc<std::sync::atomic::AtomicU64>);
        impl<R: Read> Read for Counted<R> {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let k = self.0.read(b)?;
                self.1.fetch_add(k as u64, std::sync::atomic::Ordering::Relaxed);
                Ok(k)
            }
        }
        rc.fetch_add(first.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let mut src = Counted(r, rc);
        let mut w = ChanWriter { tx, buf: Vec::new() };
        if compressible {
            let mut enc = zstd::Encoder::new(&mut w, 1).map_err(|e| e.to_string())?;
            let _ = enc.multithread(4);
            enc.write_all(&first).map_err(|e| e.to_string())?;
            std::io::copy(&mut src, &mut enc).map_err(|e| e.to_string())?;
            enc.finish().map_err(|e| e.to_string())?;
        } else {
            w.write_all(&first).map_err(|e| e.to_string())?;
            std::io::copy(&mut src, &mut w).map_err(|e| e.to_string())?;
        }
        if !w.buf.is_empty() {
            let last = std::mem::take(&mut w.buf);
            w.tx.blocking_send(last).map_err(|_| "upload gone".to_string())?;
        }
        Ok(())
    });

    let upload = store.put_multipart(&S3Path::from(key)).await.map_err(|e| e.to_string())?;
    let mut w = object_store::WriteMultipart::new_with_chunk_size(upload, CHUNK);
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mode: &[u8] = if compressible { b"z" } else { b"r" };
    sha2::Digest::update(&mut hasher, mode);
    w.write(mode);
    let mut comp = 1u64;
    while let Some(chunk) = rx.recv().await {
        comp += chunk.len() as u64;
        sha2::Digest::update(&mut hasher, &chunk);
        w.wait_for_capacity(10).await.map_err(|e| e.to_string())?;
        w.write(&chunk);
    }
    producer.join().map_err(|_| "producer panicked".to_string())??;
    w.finish().await.map_err(|e| e.to_string())?;
    Ok((raw_count.load(std::sync::atomic::Ordering::Relaxed), comp, sha_hex(hasher)))
}

/// Ref -> snapshot record -> ordered entries.
async fn remote_lineage(store: &dyn ObjectStore, ws: &str) -> Result<Vec<Entry>, String> {
    let snap_id = String::from_utf8_lossy(&get_bytes(store, &format!("refs/{ws}")).await?).to_string();
    let rec = get_bytes(store, &format!("snaps/{snap_id}.json")).await?;
    let s = String::from_utf8_lossy(&rec);
    let inner = s.split('[').nth(1).and_then(|x| x.split(']').next()).ok_or("bad record")?;
    Ok(inner
        .split(',')
        .filter(|x| !x.trim().is_empty())
        .map(|x| Entry::parse(x.trim().trim_matches('"')))
        .collect())
}

/// Write a snapshot record for `lineage` and point `ws`'s ref at it.
async fn commit(store: &dyn ObjectStore, ws: &str, lineage: &[Entry]) -> Result<(), String> {
    let snap_id = uuid();
    let rec = format!(
        "{{\"lineage\":[{}]}}",
        lineage.iter().map(|l| format!("\"{}\"", l.encode())).collect::<Vec<_>>().join(",")
    );
    put_bytes(store, &format!("snaps/{snap_id}.json"), rec.into_bytes()).await?;
    put_bytes(store, &format!("refs/{ws}"), snap_id.into_bytes()).await
}

/// Snapshot live, upload the delta, extend the lineage, move the ref.
async fn push(store: &dyn ObjectStore, pool: &Pool, ws: &str) -> Result<(), String> {
    let _lock = ws_lock(pool, ws)?;
    let mut lineage = pool.lineage(ws);
    let root = pool.snap_root(ws);
    let parent = lineage.last().map(|e| root.join(e.snap()));
    let t = Instant::now();
    let id = uuid();
    run(&[
        "btrfs", "subvolume", "snapshot", "-r",
        pool.live(ws).to_str().unwrap(),
        root.join(&id).to_str().unwrap(),
    ])?;
    let mut child = spawn_send(&root.join(&id), parent)?;
    let (raw, clen, sha) =
        upload_stream(store, &format!("layers/{id}.zst"), child.stdout.take().unwrap()).await?;
    let st = child.wait_with_output().map_err(|e| e.to_string())?;
    if !st.status.success() {
        return Err(format!("btrfs send: {}", String::from_utf8_lossy(&st.stderr)));
    }
    lineage.push(Entry::Stream { blob: id.clone(), sha });
    commit(store, ws, &lineage).await?;
    pool.set_lineage(ws, &lineage);
    println!("push: layer {id} ({} deep), {raw}B -> {clen}B, {:?}", lineage.len(), t.elapsed());

    // Auto-squash: the push itself stays fast (the delta is already durable); the block
    // layer is built by a detached child so this process returns immediately.
    let since_block = lineage
        .iter()
        .rev()
        .take_while(|e| matches!(e, Entry::Stream { .. }))
        .count();
    // Squash thresholds, tunable per deployment: WSSNAP_SQUASH_MB (delta size that forces a
    // block layer, default 256) and WSSNAP_CHAIN_MAX (stream layers since the last block
    // layer, default 50).
    let squash_mb: u64 = std::env::var("WSSNAP_SQUASH_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(256);
    let chain_max: usize = std::env::var("WSSNAP_CHAIN_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let reason = if raw > squash_mb << 20 {
        Some(format!("delta > {squash_mb}MB"))
    } else if since_block > chain_max {
        Some(format!("chain > {chain_max}"))
    } else {
        None
    };
    // The latch stops a second squash from spawning while one is still building; the squash
    // child removes it when done.
    let latch = pool.root.join("ws").join(format!("{ws}.squashing"));
    if let Some(r) = reason {
        if latch.exists() {
            println!("push: squash due ({r}) but one is already running");
        } else {
            std::fs::write(&latch, b"").map_err(|e| e.to_string())?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            Command::new(exe)
                .args(["squash", ws])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            println!("push: squash triggered ({r}), running in background");
        }
    }
    Ok(())
}

/// Materialize the ref's lineage locally, fetching only what is missing, then point live at
/// the tip. A lineage whose base is a block layer not yet local restores by image mount:
/// decompress straight to a loop-mounted fs — no per-file receive for the bulk.
async fn pull(store: Arc<dyn ObjectStore>, pool: &Pool, ws: &str, from: &str) -> Result<(), String> {
    let lineage = remote_lineage(store.as_ref(), from).await?;
    std::fs::create_dir_all(pool.recv()).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(pool.wsdir(ws)).map_err(|e| e.to_string())?;

    // Block fast path only when the base isn't already materialized on the shared pool.
    let mut snap_root = pool.recv();
    let mut rest = &lineage[..];
    if let Some(Entry::Block { blob, snap, sha }) = lineage.first() {
        if !pool.recv().join(snap).exists() {
            let wsroot = pool.wsdir(ws);
            if !is_mountpoint(&wsroot) {
                // Stream download -> decode -> disk; nothing buffers the whole image.
                let t = Instant::now();
                use futures::StreamExt;
                let mut s = store
                    .get(&S3Path::from(format!("layers/{blob}.zst")))
                    .await
                    .map_err(|e| e.to_string())?
                    .into_stream();
                std::fs::create_dir_all(pool.root.join("img")).map_err(|e| e.to_string())?;
                let img = pool.img(blob);
                let f = std::fs::File::create(&img).map_err(|e| e.to_string())?;
                let mut w = std::io::BufWriter::new(f);
                let mut dec: Option<zstd::stream::write::Decoder<_>> = None;
                let mut first = true;
                let mut raw_mode = false;
                let mut h = <sha2::Sha256 as sha2::Digest>::new();
                while let Some(b) = s.next().await {
                    let b = b.map_err(|e| e.to_string())?;
                    sha2::Digest::update(&mut h, &b);
                    let mut d: &[u8] = &b;
                    if first {
                        first = false;
                        raw_mode = d[0] == b'r';
                        d = &d[1..];
                        if !raw_mode {
                            dec = Some(
                                zstd::stream::write::Decoder::new(w).map_err(|e| e.to_string())?,
                            );
                            w = std::io::BufWriter::new(
                                std::fs::File::open("/dev/null").map_err(|e| e.to_string())?,
                            );
                        }
                    }
                    if let Some(dd) = dec.as_mut() {
                        dd.write_all(d).map_err(|e| e.to_string())?;
                    } else {
                        w.write_all(d).map_err(|e| e.to_string())?;
                    }
                }
                if let Some(mut dd) = dec.take() {
                    dd.flush().map_err(|e| e.to_string())?;
                } else {
                    w.flush().map_err(|e| e.to_string())?;
                }
                if sha_hex(h) != *sha {
                    let _ = std::fs::remove_file(&img);
                    return Err(format!("block layer {blob}: sha mismatch (corrupt image)"));
                }
                run(&["mount", "-o", "loop", img.to_str().unwrap(), wsroot.to_str().unwrap()])?;
                println!("pull: block layer {blob} restored by mount in {:?}", t.elapsed());
            }
            snap_root = wsroot;
            rest = &lineage[1..];
        }
    }

    let missing: Vec<&Entry> =
        rest.iter().filter(|e| !snap_root.join(e.snap()).exists()).collect();
    let t = Instant::now();
    let mut jobs = Vec::new();
    for e in &missing {
        let store = store.clone();
        let key = format!("layers/{}.zst", e.blob());
        jobs.push(tokio::spawn(async move { get_bytes(store.as_ref(), &key).await }));
    }
    let mut blobs = Vec::new();
    for j in jobs {
        blobs.push(j.await.map_err(|e| e.to_string())??);
    }
    let dl = t.elapsed();
    let t = Instant::now();
    for (e, b) in missing.iter().zip(&blobs) {
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut h, b);
        let got = sha_hex(h);
        if got != e.sha() {
            return Err(format!("layer {}: sha mismatch (corrupt blob)", e.blob()));
        }
        receive_into(&snap_root, b)?; // order matters: receive validates the parent-UUID chain
    }
    let tip = lineage.last().ok_or("empty lineage")?;
    if !pool.live(ws).exists() {
        run(&[
            "btrfs", "subvolume", "snapshot",
            snap_root.join(tip.snap()).to_str().unwrap(),
            pool.live(ws).to_str().unwrap(),
        ])?;
    }
    pool.set_lineage(ws, &lineage);
    println!(
        "pull: {} layers, {} fetched, download {dl:?}, receive {:?}",
        lineage.len(),
        missing.len(),
        t.elapsed()
    );
    Ok(())
}

/// Convert the local tip into a block layer: a mountable btrfs image holding the tip
/// snapshot, populated by a LOCAL send/receive (the per-file cost paid here, once, in the
/// background — never again on restore). The new lineage is that single block entry.
async fn squash(store: &dyn ObjectStore, pool: &Pool, ws: &str) -> Result<(), String> {
    let latch = pool.root.join("ws").join(format!("{ws}.squashing"));
    let r = squash_inner(store, pool, ws).await;
    let _ = std::fs::remove_file(&latch);
    r
}

async fn squash_inner(store: &dyn ObjectStore, pool: &Pool, ws: &str) -> Result<(), String> {
    let lineage = pool.lineage(ws);
    let tip = lineage.last().ok_or("no lineage; push first")?.snap().to_string();
    let root = pool.snap_root(ws);
    let t = Instant::now();

    // Size the image from the tip's content plus btrfs overhead headroom.
    let du = Command::new("du")
        .args(["-sb", root.join(&tip).to_str().unwrap()])
        .output()
        .map_err(|e| e.to_string())?;
    let used: u64 = String::from_utf8_lossy(&du.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("du failed")?;
    // Headroom: btrfs metadata for many small files is far from free, so 50% + 1G slack;
    // the image is sparse and zstd flattens the unused tail to nearly nothing.
    let size = used + used / 2 + (1 << 30);

    let blob = uuid();
    std::fs::create_dir_all(pool.root.join("img")).map_err(|e| e.to_string())?;
    let img = pool.img(&blob);
    run(&["truncate", "-s", &size.to_string(), img.to_str().unwrap()])?;
    run(&["mkfs.btrfs", "-q", "-m", "single", "-d", "single", img.to_str().unwrap()])?;
    let mnt = format!("/tmp/wssquash-{blob}");
    std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    run(&["mount", "-o", "loop", img.to_str().unwrap(), &mnt])?;
    let populate = (|| {
        let mut send = Command::new("btrfs")
            .args(["send", "-q", root.join(&tip).to_str().unwrap()])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut recv = Command::new("btrfs")
            .args(["receive", "-q", &mnt])
            .stdin(send.stdout.take().unwrap())
            .spawn()
            .map_err(|e| e.to_string())?;
        if !recv.wait().map_err(|e| e.to_string())?.success()
            || !send.wait().map_err(|e| e.to_string())?.success()
        {
            return Err("populate send/receive failed".to_string());
        }
        Ok(())
    })();
    run(&["umount", &mnt])?;
    let _ = std::fs::remove_dir(&mnt);
    populate?;
    let built = t.elapsed();

    let t = Instant::now();
    let f = std::fs::File::open(&img).map_err(|e| e.to_string())?;
    let (raw, clen, sha) = upload_stream(store, &format!("layers/{blob}.zst"), f).await?;
    // The commit races pushes that landed while the image was building: under the lock,
    // re-read the lineage and graft any streams that arrived after our tip onto the new
    // block base, so their history is preserved rather than clobbered.
    let _lock = ws_lock(pool, ws)?;
    let now = pool.lineage(ws);
    let mut new_lineage = vec![Entry::Block { blob: blob.clone(), snap: tip.clone(), sha }];
    let after: Vec<Entry> = now
        .iter()
        .skip_while(|e| e.snap() != tip)
        .skip(1)
        .cloned()
        .collect();
    new_lineage.extend(after);
    commit(store, ws, &new_lineage).await?;
    pool.set_lineage(ws, &new_lineage);
    println!(
        "squash: block layer {blob}, image {raw}B -> {clen}B, build {built:?}, upload {:?}",
        t.elapsed()
    );
    Ok(())
}

/// Clone a RUNNING workspace onto this pool, minimizing source downtime.
///
/// Phase 1 (source untouched): prefetch — pull everything up to the source's last SAVED
/// snapshot onto this pool, so the bulk transfer happens while the container keeps running.
/// Phase 2 (container lock): stop the source's containers (WSSNAP_STOP_CMD), sync, push the
/// final delta — small by construction, the prefetch absorbed the rest — then restart
/// (WSSNAP_START_CMD) as soon as that delta is durable.
/// Phase 3: point the clone's ref at the frozen snapshot and fetch just that last delta.
///
/// Creating a container from an existing snapshot needs none of this — that is `fork`.
async fn clone_ws(
    store: Arc<dyn ObjectStore>,
    target: &Pool,
    src_pool: &Pool,
    src: &str,
    dst: &str,
) -> Result<(), String> {
    let stop = std::env::var("WSSNAP_STOP_CMD")
        .map_err(|_| "clone requires WSSNAP_STOP_CMD (how to stop the source containers)")?;
    let t0 = Instant::now();

    // Phase 1: warm this pool up to the last saved snapshot; source keeps running.
    pull(store.clone(), target, dst, src).await?;
    let prefetched = t0.elapsed();

    // Phase 2: the locked window — only the final delta happens inside it.
    let t1 = Instant::now();
    run(&["sh", "-c", &stop])?;
    run(&["sync", "-f", src_pool.live(src).to_str().unwrap()])?;
    push(store.as_ref(), src_pool, src).await?;
    if let Ok(start) = std::env::var("WSSNAP_START_CMD") {
        run(&["sh", "-c", &start])?;
    }
    let locked = t1.elapsed();

    // Phase 3: re-point the clone at the frozen snapshot and apply the one missing delta.
    let snap_ref = get_bytes(store.as_ref(), &format!("refs/{src}")).await?;
    put_bytes(store.as_ref(), &format!("refs/{dst}"), snap_ref).await?;
    run(&["btrfs", "subvolume", "delete", target.live(dst).to_str().unwrap()])?;
    pull(store, target, dst, dst).await?;
    println!(
        "clone: {dst} from running {src}; prefetch {prefetched:?}, source locked {locked:?}, total {:?}",
        t0.elapsed()
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pool = Pool::new(&std::env::var("POOL").unwrap_or_else(|_| "/mnt/hosta".into()));
    let usage = "usage: wssnap {init|push|pull} <ws> | wssnap {fork|clone} <src> <new>";
    let (cmd, name) = match (args.get(1), args.get(2)) {
        (Some(c), Some(n)) => (c.as_str(), n.as_str()),
        _ => return eprintln!("{usage}"),
    };
    let store = s3();

    let r = match cmd {
        "init" => {
            let prep = std::fs::create_dir_all(pool.wsdir(name))
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    run(&["btrfs", "subvolume", "create", pool.live(name).to_str().unwrap()])?;
                    std::fs::create_dir_all(pool.recv()).map_err(|e| e.to_string())
                });
            match prep {
                Ok(()) => push(store.as_ref(), &pool, name).await,
                e => e,
            }
        }
        "push" => push(store.as_ref(), &pool, name).await,
        "pull" => pull(store, &pool, name, name).await,
        // Internal: spawned in the background by push when a squash trigger trips.
        "squash" => squash(store.as_ref(), &pool, name).await,
        "fork" => match args.get(3) {
            Some(dst) => match get_bytes(store.as_ref(), &format!("refs/{name}")).await {
                Ok(snap_ref) => match put_bytes(store.as_ref(), &format!("refs/{dst}"), snap_ref).await {
                    Ok(()) => pull(store, &pool, dst, name).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            None => Err("usage: wssnap fork <src-ws> <new-ws>".into()),
        },
        // clone <src> <new>: copy a RUNNING workspace. POOL is the target pool; SRC_POOL
        // (default /mnt/hosta) is where the live source runs.
        "clone" => match args.get(3) {
            Some(dst) => {
                let sp = Pool::new(&std::env::var("SRC_POOL").unwrap_or_else(|_| "/mnt/hosta".into()));
                clone_ws(store, &pool, &sp, name, dst).await
            }
            None => Err("usage: wssnap clone <src-ws> <new-ws>".into()),
        },
        _ => return eprintln!("{usage}"),
    };
    if let Err(e) = r {
        eprintln!("wssnap: {e}");
        std::process::exit(1);
    }
}
