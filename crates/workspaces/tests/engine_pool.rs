//! Engine pool/blob tests. The `upload_stream`/`get_bytes` round trip runs everywhere against
//! `InMemory`. Everything that touches btrfs is gated on `have_btrfs()` (root + the binary on
//! PATH) and skips cleanly on this Mac and on any non-root runner.

use object_store::ObjectStore;
use object_store::memory::InMemory;
use rand::RngCore;
use rustic_git_workspaces::engine::blob;
use rustic_git_workspaces::engine::{Pool, have_btrfs, ws_lock};
use rustic_git_workspaces::model::{LayerKind, LineageEntry};
use std::io::Cursor;
use std::sync::Arc;

/// Whole-object read. Only the tests want one — production streams every blob.
async fn get_bytes(store: &dyn ObjectStore, key: &str) -> Result<Vec<u8>, String> {
    let mut s = blob::get_stream(store, key).await?;
    let mut out = Vec::new();
    while let Some(b) = blob::next_chunk(key, &mut s).await? {
        out.extend_from_slice(&b);
    }
    Ok(out)
}

#[tokio::test]
async fn upload_stream_roundtrip_text_is_compressed() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let text = "the quick brown fox ".repeat(10_000);
    let (raw, _clen, sha) =
        blob::upload_stream(store.as_ref(), "layers/text.zst", Cursor::new(text.clone().into_bytes()))
            .await
            .unwrap();
    assert_eq!(raw, text.len() as u64);

    let got = get_bytes(store.as_ref(), "layers/text.zst").await.unwrap();
    assert_eq!(got[0], b'z', "compressible payload must use zstd mode");
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, &got);
    assert_eq!(blob::sha_hex(h), sha);

    let mut dec = zstd::Decoder::new(&got[1..]).unwrap();
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut out).unwrap();
    assert_eq!(out, text.into_bytes());
}

#[tokio::test]
async fn upload_stream_roundtrip_random_is_raw() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut payload = vec![0u8; 1 << 20];
    rand::thread_rng().fill_bytes(&mut payload);
    let (raw, _clen, sha) =
        blob::upload_stream(store.as_ref(), "layers/rand.zst", Cursor::new(payload.clone()))
            .await
            .unwrap();
    assert_eq!(raw, payload.len() as u64);

    let got = get_bytes(store.as_ref(), "layers/rand.zst").await.unwrap();
    assert_eq!(got[0], b'r', "incompressible payload must skip zstd");
    assert_eq!(&got[1..], payload.as_slice());
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, &got);
    assert_eq!(blob::sha_hex(h), sha);
}

#[test]
fn lineage_entry_encode_parse_roundtrip() {
    let stream = LineageEntry { kind: LayerKind::Stream, blob: "b1".into(), snap: None, sha256: "abc".into(), unpushed: false };
    assert_eq!(stream.encode(), "s:b1:abc");
    assert_eq!(LineageEntry::parse(&stream.encode()).unwrap().encode(), stream.encode());
    assert_eq!(stream.snap_name(), "b1");

    let block = LineageEntry {
        kind: LayerKind::Block,
        blob: "b2".into(),
        snap: Some("s2".into()),
        sha256: "def".into(),
        unpushed: false,
    };
    assert_eq!(block.encode(), "b:b2:s2:def");
    assert_eq!(LineageEntry::parse(&block.encode()).unwrap().encode(), block.encode());
    assert_eq!(block.snap_name(), "s2");
}

#[test]
fn unpushed_marker_survives_encode_parse_and_old_lines_default_to_pushed() {
    let unpushed = LineageEntry { kind: LayerKind::Stream, blob: "b3".into(), snap: None, sha256: "ghi".into(), unpushed: true };
    assert_eq!(unpushed.encode(), "s:b3:ghi|u");
    let back = LineageEntry::parse(&unpushed.encode()).unwrap();
    assert!(back.unpushed);
    assert_eq!(back.sha256, "ghi");

    // A line written before commit/push existed has no `|u` suffix and must parse as pushed.
    assert!(!LineageEntry::parse("s:b1:abc").unwrap().unpushed);
}

/// A loopback btrfs pool backed by a truncated sparse image, mounted for the test and torn
/// down (unmount + remove) on drop. Root only — construction panics if mkfs/mount fail, which
/// is fine since callers only build this behind `have_btrfs()`.
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
        run(&["truncate", "-s", "2G", img.to_str().unwrap()]);
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

#[test]
fn btrfs_snapshot_send_receive_roundtrip() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }

    let src = LoopbackPool::new();
    let dst = LoopbackPool::new();

    let ws = "wsa";
    std::fs::create_dir_all(src.pool.voldir(ws)).unwrap();
    run(&["btrfs", "subvolume", "create", src.pool.live(ws).to_str().unwrap()]);
    std::fs::write(src.pool.live(ws).join("hello.txt"), b"hello from the source subvolume").unwrap();

    let _lock = ws_lock(&src.pool, ws).unwrap();
    let snap_id = "snap-1";
    let snap_path = src.pool.recv().join(snap_id);
    run(&[
        "btrfs",
        "subvolume",
        "snapshot",
        "-r",
        src.pool.live(ws).to_str().unwrap(),
        snap_path.to_str().unwrap(),
    ]);
    drop(_lock);

    let mut child = blob::spawn_send(&snap_path, None, &[]).unwrap();
    let mut sent = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take().unwrap(), &mut sent).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "btrfs send failed: {}", String::from_utf8_lossy(&out.stderr));

    // Emulate the layer blob's leading mode byte with raw ('r') encoding, as `upload_stream`
    // would produce for an incompressible payload.
    let mut layer = vec![b'r'];
    layer.extend_from_slice(&sent);
    blob::receive_into(&dst.pool.recv(), &layer[..]).unwrap();

    let received = dst.pool.recv().join(snap_id).join("hello.txt");
    assert_eq!(std::fs::read(received).unwrap(), b"hello from the source subvolume");
}

/// An `InMemory` whose bodies arrive one slow chunk at a time — a throttled link, on paused time.
#[derive(Debug)]
struct SlowStore {
    inner: InMemory,
    chunk: usize,
    gap: std::time::Duration,
    /// Single-PUT writes seen — the path a layer past the 5 GiB ceiling cannot take.
    single_puts: std::sync::atomic::AtomicUsize,
}
impl std::fmt::Display for SlowStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SlowStore")
    }
}
#[async_trait::async_trait]
impl ObjectStore for SlowStore {
    async fn put_opts(
        &self,
        p: &object_store::path::Path,
        payload: object_store::PutPayload,
        o: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.single_puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.put_opts(p, payload, o).await
    }
    async fn put_multipart_opts(
        &self,
        p: &object_store::path::Path,
        o: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(p, o).await
    }
    async fn get_opts(
        &self,
        p: &object_store::path::Path,
        o: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        use futures::StreamExt;
        let r = self.inner.get_opts(p, o).await?;
        let (meta, range, attributes, extensions) =
            (r.meta.clone(), r.range.clone(), r.attributes.clone(), r.extensions.clone());
        let all = r.bytes().await?;
        let chunks: Vec<_> =
            (0..all.len()).step_by(self.chunk).map(|i| all.slice(i..(i + self.chunk).min(all.len()))).collect();
        let gap = self.gap;
        let payload = object_store::GetResultPayload::Stream(
            futures::stream::iter(chunks)
                .then(move |c| async move {
                    tokio::time::sleep(gap).await;
                    Ok(c)
                })
                .boxed(),
        );
        Ok(object_store::GetResult { payload, meta, range, attributes, extensions })
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        o: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, o).await
    }
}

/// The audit's P-11: a layer whose body takes longer than `GET_TIMEOUT` end to end must still
/// arrive — the deadline is per chunk, so a slow link is merely slow. Only a link that goes SILENT
/// for that long is an error, and that error is transient (no `FETCH_FAILED` marker), unlike a
/// blob the store says is not there. On paused time the whole thing runs in milliseconds.
#[tokio::test(start_paused = true)]
async fn a_slow_body_is_read_per_chunk_and_only_a_silent_one_times_out() {
    let inner = InMemory::new();
    let key = "layers/slow.zst";
    let payload = vec![7u8; 4096];
    blob::put_bytes(&inner, key, payload.clone()).await.unwrap();

    // Four chunks, each arriving just inside the deadline: 4 × 100 s of wall time, well past the
    // 120 s that used to bound the whole body.
    let slow = SlowStore {
        inner,
        chunk: 1024,
        gap: blob::GET_TIMEOUT - std::time::Duration::from_secs(20),
        single_puts: Default::default(),
    };
    assert_eq!(get_bytes(&slow, key).await.unwrap(), payload);

    let stalled = SlowStore {
        inner: InMemory::new(),
        chunk: 1024,
        gap: blob::GET_TIMEOUT + std::time::Duration::from_secs(1),
        single_puts: Default::default(),
    };
    blob::put_bytes(&stalled.inner, key, payload).await.unwrap();
    let err = get_bytes(&stalled, key).await.unwrap_err();
    assert!(err.contains("stalled"), "{err}");
    assert!(!err.contains(rustic_git_workspaces::engine::ops::FETCH_FAILED), "a stall is transient: {err}");

    let err = get_bytes(&InMemory::new(), "layers/absent.zst").await.unwrap_err();
    assert!(err.contains(rustic_git_workspaces::engine::ops::FETCH_FAILED), "a miss is permanent: {err}");
}

/// The audit's P-12/P-13/Q-11: a staged layer goes up in multipart parts (never one PUT, which
/// S3/Azure cap at 5 GiB), and a restore's layer comes down a chunk at a time straight to disk,
/// with the sha computed on the way — nothing holds a whole layer in memory on either side.
#[tokio::test]
async fn a_staged_layer_uploads_multipart_and_restores_to_disk_by_chunk() {
    let store = SlowStore { inner: InMemory::new(), chunk: 1024, gap: Default::default(), single_puts: Default::default() };
    let tmp = tempfile::tempdir().unwrap();
    let mut layer = vec![b'r'];
    let mut body = vec![0u8; 100 * 1024];
    rand::thread_rng().fill_bytes(&mut body);
    layer.extend_from_slice(&body);
    let staged = tmp.path().join("staged.zst");
    std::fs::write(&staged, &layer).unwrap();

    blob::upload_file(&store, "layers/x.zst", &staged).await.unwrap();
    assert_eq!(store.single_puts.load(std::sync::atomic::Ordering::Relaxed), 0, "must be multipart");
    assert_eq!(get_bytes(&store, "layers/x.zst").await.unwrap(), layer);

    let dest = tmp.path().join("x.layer");
    let sha = blob::get_to_file(&store, "layers/x.zst", &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), layer);
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, &layer);
    assert_eq!(sha, blob::sha_hex(h));
}

/// The generation file is the timer's whole memory: absent means "never pushed" (push), a number
/// means "push only if the disk moved past it". Written tmp+rename like the lineage, so a crash
/// mid-write reads as absent — one extra push, never a skipped one.
#[test]
fn the_pushed_generation_round_trips_and_is_absent_until_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Pool::new(tmp.path());
    std::fs::create_dir_all(pool.voldir("home-alice")).unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), None);
    pool.record_pushed_gen("home-alice", 4711).unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), Some(4711));
    assert_eq!(pool.pushed_gen_path("home-alice"), tmp.path().join("vol/home-alice/.pushed-gen"));
    assert!(!tmp.path().join("vol/home-alice/.pushed-gen.tmp").exists());
    std::fs::write(pool.pushed_gen_path("home-alice"), b"garbage").unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), None, "unreadable is absent, which pushes");
    assert!(pool.record_pushed_gen("nowhere", 1).is_err(), "a missing voldir is an error, not a panic");
}
