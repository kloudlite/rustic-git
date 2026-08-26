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

#[tokio::test]
async fn upload_stream_roundtrip_text_is_compressed() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let text = "the quick brown fox ".repeat(10_000);
    let (raw, _clen, sha) =
        blob::upload_stream(store.as_ref(), "layers/text.zst", Cursor::new(text.clone().into_bytes()))
            .await
            .unwrap();
    assert_eq!(raw, text.len() as u64);

    let got = blob::get_bytes(store.as_ref(), "layers/text.zst").await.unwrap();
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

    let got = blob::get_bytes(store.as_ref(), "layers/rand.zst").await.unwrap();
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

    let mut child = blob::spawn_send(&snap_path, None).unwrap();
    let mut sent = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take().unwrap(), &mut sent).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "btrfs send failed: {}", String::from_utf8_lossy(&out.stderr));

    // Emulate the layer blob's leading mode byte with raw ('r') encoding, as `upload_stream`
    // would produce for an incompressible payload.
    let mut layer = vec![b'r'];
    layer.extend_from_slice(&sent);
    blob::receive_into(&dst.pool.recv(), &layer).unwrap();

    let received = dst.pool.recv().join(snap_id).join("hello.txt");
    assert_eq!(std::fs::read(received).unwrap(), b"hello from the source subvolume");
}
