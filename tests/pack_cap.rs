//! Its own binary: `RUSTIC_GIT_MAX_BODY` is process-global and every other push test would
//! trip over a 1 KiB cap.
mod common;
use rustic_git_core::pktline;
use rustic_git_git::protocol::receive;
use std::io::Cursor;

use common::pack_of;

/// The SSH path has no HTTP body limit in front of it; the pack reader itself must refuse.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_pack_is_refused_before_it_is_indexed() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    std::env::set_var("RUSTIC_GIT_MAX_BODY", "1024");
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();

    let d = tempfile::tempdir().unwrap();
    common::git(d.path(), &["init", "-q", "-b", "main"]);
    // Incompressible, so the pack is comfortably past 1 KiB.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut body = Vec::with_capacity(8192);
    while body.len() < 8192 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        body.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(d.path().join("big.bin"), &body).unwrap();
    common::git(d.path(), &["add", "."]);
    common::git(d.path(), &["commit", "-qm", "big"]);
    let head = common::git(d.path(), &["rev-parse", "HEAD"]);
    let pack = pack_of(d.path(), &format!("{head}\n"));
    assert!(pack.len() > 1024);

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack);

    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let s2 = s.clone();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(&s2, &repo, &mut Cursor::new(req), &mut out, &Default::default()).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("unpack error"), "the push must be refused: {text}");
    assert!(text.contains("size limit"), "and say why: {text}");
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());
    // `.pruned` is the stale-pack scan's own hourly gate marker, not part of the refused pack.
    let left: Vec<_> = std::fs::read_dir(&repo.pack_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != ".pruned")
        .collect();
    assert_eq!(left, Vec::<std::ffi::OsString>::new(), "nothing of the refused pack stays on disk");
}
