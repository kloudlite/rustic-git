//! Blob IO: layer stores, streaming compressed upload/download, and btrfs send/receive glue.

use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path as S3Path};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

pub fn sha_hex(h: sha2::Sha256) -> String {
    use sha2::Digest;
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Azure Blob layer store for one region: account/key/container come from the region's
/// Cosmos record (Task 2's `model::Region`).
pub fn region_store(account: &str, key: &str, container: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        object_store::azure::MicrosoftAzureBuilder::new()
            .with_account(account)
            .with_access_key(key)
            .with_container_name(container)
            .build()
            .expect("build azure object store"),
    )
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
            .build()
            .expect("build s3 object store"),
    )
}

pub async fn get_bytes(store: &dyn ObjectStore, key: &str) -> Result<Vec<u8>, String> {
    Ok(store
        .get(&S3Path::from(key))
        .await
        .map_err(|e| format!("{key}: {e}"))?
        .bytes()
        .await
        .map_err(|e| e.to_string())?
        .to_vec())
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
