mod common;
use rustic_git_core::pktline;
use rustic_git_git::protocol::{receive, upload};
use std::io::Cursor;
use std::sync::atomic::Ordering;

use common::pack_of;

fn push(
    s: &std::sync::Arc<rustic_git_storage::store::Store>,
    repo: &rustic_git_storage::store::Repo,
    old: &str,
    new: &str,
    pack: Vec<u8>,
) -> String {
    let mut req = Vec::new();
    pktline::write_pkt(&mut req, format!("{old} {new} refs/heads/main\0report-status").as_bytes())
        .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack);
    let mut out = Vec::new();
    receive::serve(s, repo, &mut Cursor::new(req), &mut out, &Default::default()).unwrap();
    String::from_utf8_lossy(&out).to_string()
}

fn fetch(
    s: &std::sync::Arc<rustic_git_storage::store::Store>,
    repo: &rustic_git_storage::store::Repo,
    lines: &[String],
) -> String {
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    for l in lines {
        pktline::write_text(&mut req, l).unwrap();
    }
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let mut out = Vec::new();
    upload::serve(s, repo, &mut Cursor::new(req), &mut out, &Default::default()).unwrap();
    String::from_utf8_lossy(&out).to_string()
}

/// "Does this repo have X" used to be answered by enumerating every object reachable from every
/// ref — on each fetch with a `have`, and on each push whose tree kept anything unchanged, i.e.
/// every push. Both must cost the size of the change, not of the repo. Its own test binary,
/// because the counter is process-wide.
#[tokio::test(flavor = "multi_thread")]
async fn reachability_costs_the_change_not_the_repo() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();

    // Deep history over a small tree: the repo is many objects, one commit's worth is few.
    let d = tempfile::tempdir().unwrap();
    common::git(d.path(), &["init", "-q", "-b", "main"]);
    common::git(d.path(), &["config", "user.email", "t@t"]);
    common::git(d.path(), &["config", "user.name", "t"]);
    std::fs::create_dir(d.path().join("src")).unwrap();
    for i in 0..300 {
        std::fs::write(d.path().join("src/f.txt"), format!("{i}\n")).unwrap();
        common::git(d.path(), &["add", "."]);
        common::git(d.path(), &["commit", "-qm", &format!("c{i}")]);
    }
    let head = common::git(d.path(), &["rev-parse", "HEAD"]);
    let parent = common::git(d.path(), &["rev-parse", "HEAD~1"]);
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let (s2, r2, h2) = (s.clone(), repo.clone(), head.clone());
    let pack = pack_of(d.path(), &format!("{head}\n"));
    let text = tokio::task::spawn_blocking(move || push(&s2, &r2, &"0".repeat(40), &h2, pack))
        .await
        .unwrap();
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let total = {
        let odb = repo.odb().unwrap();
        odb.iter().unwrap().count()
    };
    assert!(total > 600, "fixture is big enough to measure: {total}");

    // An incremental fetch: the have is the parent, one step below the tip.
    let before = upload::WALKED.load(Ordering::Relaxed);
    let (s2, r2) = (s.clone(), repo.clone());
    let (h2, p2) = (head.clone(), parent.clone());
    let text = tokio::task::spawn_blocking(move || fetch(&s2, &r2, &[format!("want {h2}"), format!("have {p2}")]))
        .await
        .unwrap();
    assert!(text.contains("packfile"), "{text}");
    let fetch_walked = upload::WALKED.load(Ordering::Relaxed) - before;

    // A push of one commit on top.
    std::fs::write(d.path().join("src/f.txt"), "new\n").unwrap();
    common::git(d.path(), &["commit", "-qam", "one more"]);
    let new = common::git(d.path(), &["rev-parse", "HEAD"]);
    let before = upload::WALKED.load(Ordering::Relaxed);
    let (s2, r2, h2, n2) = (s.clone(), repo.clone(), head.clone(), new.clone());
    let pack = pack_of(d.path(), &format!("{new}\n^{head}\n"));
    let text = tokio::task::spawn_blocking(move || push(&s2, &r2, &h2, &n2, pack))
        .await
        .unwrap();
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let push_walked = upload::WALKED.load(Ordering::Relaxed) - before;

    eprintln!("repo objects {total}; walked: fetch {fetch_walked}, push {push_walked}");
    assert!(fetch_walked * 20 < total, "fetch walked {fetch_walked} of {total} objects");
    assert!(push_walked * 20 < total, "push walked {push_walked} of {total} objects");
}
