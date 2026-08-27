//! Blob IO: layer stores, streaming compressed upload/download, and btrfs send/receive glue.

use crate::model::LayerKind;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path as S3Path};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Sidecar written next to every layer blob at `layers/{uuid}.json`, so fsck can rebuild
/// lineage from object-store listings alone when `Snapshot` docs are lost. `parent_blob` is
/// the blob id of the layer this one was pushed/squashed on top of (`None` for a lineage root);
/// `snap_uuid` is the local RO snapshot name a block layer materializes (`LineageEntry.snap`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerSidecar {
    pub kind: LayerKind,
    pub parent_blob: Option<String>,
    pub snap_uuid: Option<String>,
    pub sha256: String,
    pub raw: u64,
    pub stored: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Written right after `upload_stream` returns and before the record commit: a crash in
/// between leaves an orphan blob+sidecar, which is safe — fsck still finds it, as a degenerate
/// single-entry candidate tip nothing else chains onto, but nobody has reason to `adopt` a
/// 1-layer tip over the real lineage.
pub async fn write_sidecar(store: &dyn ObjectStore, blob_id: &str, s: &LayerSidecar) -> Result<(), String> {
    let bytes = serde_json::to_vec(s).map_err(|e| e.to_string())?;
    put_bytes(store, &format!("layers/{blob_id}.json"), bytes).await
}

pub fn sha_hex(h: sha2::Sha256) -> String {
    use sha2::Digest;
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// How long one blob object may take, and how hard object_store retries under that.
///
/// Both are bounds on the SAME failure: a store that answers neither yes nor no. The default
/// retry budget is three minutes of invisible waiting, and nothing above it had a deadline at
/// all — a restore reading a container it cannot see sat in `phase: working` with the message
/// "btrfs operation in flight" until someone looked. A blob is either fetched or it is an error.
/// ponytail: one flat per-object deadline, generous enough for a 32 MB chunk on a slow uplink;
/// make it a function of the layer's stored size if a real layer ever legitimately exceeds it.
pub const GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn retry() -> object_store::RetryConfig {
    object_store::RetryConfig {
        max_retries: 3,
        retry_timeout: GET_TIMEOUT,
        ..Default::default()
    }
}

/// Azure Blob layer store for one region: account/key/container come from the region's
/// Cosmos record (Task 2's `model::Region`).
pub fn region_store(account: &str, key: &str, container: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        object_store::azure::MicrosoftAzureBuilder::new()
            .with_account(account)
            .with_access_key(key)
            .with_container_name(container)
            .with_retry(retry())
            .build()
            .expect("build azure object store"),
    )
}

/// Per-region credentials the agent's Secret carries for regions OTHER than its own, as
/// `AZURE_REGION_<ID>_ACCOUNT` / `_KEY` / `_CONTAINER` with the region id uppercased and `-`
/// replaced by `_` (`centralindia-vm` → `AZURE_REGION_CENTRALINDIA_VM_*`).
///
/// Env, not Cosmos: a `Region` record deliberately carries no account KEY, and giving the agent
/// a Cosmos writer's view of every region's secrets to read one of them is a larger blast radius
/// than a Secret key per region it is actually allowed to read.
/// ponytail: one env triple per extra region, so a new region is a Secret edit and a pod restart;
/// a per-region Kubernetes Secret projected by the controller is the upgrade if that stops
/// scaling.
pub fn region_stores_from_env() -> std::collections::HashMap<String, Arc<dyn ObjectStore>> {
    region_triples(std::env::vars())
        .into_iter()
        .map(|(id, a, k, c)| (id, region_store(&a, &k, &c)))
        .collect()
}

/// The pure half of `region_stores_from_env`, so the naming rule has a test that does not have to
/// mutate the process environment. An incomplete triple is skipped and logged, never half-built.
fn region_triples(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String, String, String)> {
    let all: std::collections::HashMap<String, String> = vars.collect();
    let mut out = vec![];
    for (k, account) in &all {
        let Some(id) = k.strip_prefix("AZURE_REGION_").and_then(|r| r.strip_suffix("_ACCOUNT")) else { continue };
        let (Some(key), Some(container)) =
            (all.get(&format!("AZURE_REGION_{id}_KEY")), all.get(&format!("AZURE_REGION_{id}_CONTAINER")))
        else {
            tracing::warn!(region = %id, "AZURE_REGION_*_ACCOUNT without a matching _KEY/_CONTAINER; ignoring");
            continue;
        };
        out.push((id.to_ascii_lowercase().replace('_', "-"), account.clone(), key.clone(), container.clone()));
    }
    out
}


/// MinIO/S3 fallback for tests: `S3_URL` (default local MinIO), fixed dev creds.
pub fn s3_store() -> Arc<dyn ObjectStore> {
    Arc::new(
        object_store::aws::AmazonS3Builder::new()
            .with_endpoint(std::env::var("S3_URL").unwrap_or("http://127.0.0.1:9000".into()))
            .with_bucket_name("wslayers")
            .with_access_key_id("admin")
            .with_secret_access_key("adminadmin")
            .with_allow_http(true)
            .with_region("us-east-1")
            .with_retry(retry())
            .build()
            .expect("build s3 object store"),
    )
}

/// Whole-object read, under `GET_TIMEOUT`. Every layer fetch in the restore/pull path comes
/// through here, so the deadline lives here rather than at each call site.
pub async fn get_bytes(store: &dyn ObjectStore, key: &str) -> Result<Vec<u8>, String> {
    deadline(key, async {
        Ok(store
            .get(&S3Path::from(key))
            .await
            .map_err(|e| format!("{key}: {e}"))?
            .bytes()
            .await
            .map_err(|e| e.to_string())?
            .to_vec())
    })
    .await
}

/// `GET_TIMEOUT` around one object-store await, with the key in the message — a timeout that
/// does not say what it was reading is the same silence, one layer up.
pub async fn deadline<T>(key: &str, f: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    match tokio::time::timeout(GET_TIMEOUT, f).await {
        Ok(r) => r,
        Err(_) => Err(format!("{key}: timed out after {}s", GET_TIMEOUT.as_secs())),
    }
}

pub async fn put_bytes(store: &dyn ObjectStore, key: &str, b: Vec<u8>) -> Result<(), String> {
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
pub async fn upload_stream(
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
    let sample_len = first.len().min(4 << 20);
    let compressible = zstd::bulk::compress(&first[..sample_len], 1)
        .map(|c| (c.len() as f64) < 0.97 * (sample_len.clamp(1, 4 << 20) as f64))
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

/// Counts bytes and hashes them (post-compression, matching `upload_stream`'s hash-of-stored-
/// bytes convention) while writing to a local file — the sink `compress_to_file` drives either
/// directly (raw mode) or through a `zstd::Encoder` (compressed mode).
struct CountHash {
    f: std::io::BufWriter<std::fs::File>,
    h: sha2::Sha256,
    n: u64,
}
impl Write for CountHash {
    fn write(&mut self, d: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        self.h.update(d);
        self.n += d.len() as u64;
        self.f.write_all(d)?;
        Ok(d.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.f.flush()
    }
}

/// Local-only twin of `upload_stream`: same mode-detection and zstd-compression shape, but the
/// (mode-byte + compressed) bytes land in `dest` on disk instead of an object-store multipart —
/// this is what `push`'s internal staging phase uses so the snapshot step never touches the
/// network. Returns
/// `(raw, stored, sha256)` with the same meaning as `upload_stream`'s tuple; `push` later
/// uploads `dest` verbatim, so the sha computed here is exactly what `pull_core`'s corruption
/// check re-derives from the downloaded bytes.
pub fn compress_to_file(mut r: impl Read, dest: &Path) -> Result<(u64, u64, String), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
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
    let sample_len = first.len().min(4 << 20);
    let compressible = zstd::bulk::compress(&first[..sample_len], 1)
        .map(|c| (c.len() as f64) < 0.97 * (sample_len.clamp(1, 4 << 20) as f64))
        .unwrap_or(true);

    let f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut raw: u64 = first.len() as u64;
    let mut ch = CountHash { f: std::io::BufWriter::new(f), h: <sha2::Sha256 as sha2::Digest>::new(), n: 0 };
    let mode: &[u8] = if compressible { b"z" } else { b"r" };
    ch.write_all(mode).map_err(|e| e.to_string())?;
    if compressible {
        let mut enc = zstd::Encoder::new(&mut ch, 1).map_err(|e| e.to_string())?;
        let _ = enc.multithread(4);
        enc.write_all(&first).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let k = r.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            raw += k as u64;
            enc.write_all(&buf[..k]).map_err(|e| e.to_string())?;
        }
        enc.finish().map_err(|e| e.to_string())?;
    } else {
        ch.write_all(&first).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let k = r.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            raw += k as u64;
            ch.write_all(&buf[..k]).map_err(|e| e.to_string())?;
        }
    }
    ch.flush().map_err(|e| e.to_string())?;
    Ok((raw, ch.n, sha_hex(ch.h)))
}

/// Uploads an already-compressed local file (as `compress_to_file` wrote it) verbatim — no
/// re-compression, no re-hashing. `push` uses this for staged commits; whole-file `read` is a
/// deliberate simplification (layers here are the delta/squash-threshold size, not unbounded)
/// over a second streaming path — upgrade to a chunked read if a single layer routinely exceeds
/// memory.
/// ponytail: whole-file read, add multipart-from-file streaming if layer sizes force it.
pub async fn upload_file(store: &dyn ObjectStore, key: &str, path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    put_bytes(store, key, bytes).await
}

/// Spawn `btrfs send` for the snapshot at `path` (incremental against `parent` when given),
/// handing back the child so its stdout can stream straight into the uploader.
pub fn spawn_send(path: &Path, parent: Option<PathBuf>) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("btrfs");
    cmd.args(["send", "-q"]);
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    cmd.arg(path);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())
}

/// Decode a layer blob (leading mode byte, then zstd or raw) into `btrfs receive` at `dir`.
pub fn receive_into(dir: &Path, blob: &[u8]) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_region_triple_maps_back_to_the_region_id_and_an_incomplete_one_is_skipped() {
        let vars = [
            ("AZURE_REGION_CENTRALINDIA_VM_ACCOUNT", "acct"),
            ("AZURE_REGION_CENTRALINDIA_VM_KEY", "k"),
            ("AZURE_REGION_CENTRALINDIA_VM_CONTAINER", "wslayers"),
            ("AZURE_REGION_HALFDONE_ACCOUNT", "acct2"),
            ("AZURE_ACCOUNT", "own-region-account"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()));
        let got = super::region_triples(vars.into_iter());
        assert_eq!(
            got,
            vec![("centralindia-vm".into(), "acct".into(), "k".into(), "wslayers".into())],
            "only the complete triple, keyed by the region id as a CommitRecord spells it"
        );
    }
}
