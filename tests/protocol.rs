mod common;
use rustic_git_core::pktline;
use rustic_git_git::gc::RepackExt;
use rustic_git_gitbase::refs::UpdateRefsExt;
use rustic_git_git::protocol::{receive, upload};
use std::io::{Cursor, Write};

/// A ref name that isn't valid UTF-8 must be rejected outright, not silently rewritten
/// with U+FFFD (`from_utf8_lossy`) — otherwise the ref git's client thinks it pushed and
/// the ref actually stored diverge.
#[tokio::test(flavor = "multi_thread")]
async fn receive_rejects_non_utf8_ref_name() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();

    let mut cmd = Vec::new();
    cmd.extend_from_slice("0".repeat(40).as_bytes());
    cmd.push(b' ');
    cmd.extend_from_slice("1".repeat(40).as_bytes());
    cmd.push(b' ');
    cmd.extend_from_slice(b"refs/heads/\xff\xfe\0report-status");

    let mut req = Vec::new();
    pktline::write_pkt(&mut req, &cmd).unwrap();
    pktline::write_flush(&mut req).unwrap();

    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let result = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
    })
    .await
    .unwrap();
    assert!(result.is_err(), "non-UTF-8 ref name must be rejected, not lossily decoded");
}

/// The `\x1b[` escaping check for logged push options: Debug-formatting a `String`
/// (`{:?}`, used at the `eprintln!` in receive.rs) must never emit a raw ESC byte, since
/// that's the vector for terminal/log injection from an attacker-controlled `-o` value.
#[test]
fn push_option_debug_format_escapes_control_bytes() {
    let hostile = vec!["\x1b[31mFAKE ERROR\x1b[0m".to_string()];
    let logged = format!("{hostile:?}");
    assert!(
        !logged.as_bytes().contains(&0x1b),
        "debug-formatted push options must not contain a raw ESC byte: {logged:?}"
    );
    assert!(logged.contains("\\u{1b}"), "expected an escaped representation: {logged}");
}

/// Build a local repo with one commit; return (dir, head oid).
fn local_repo() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    common::git(d.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
    common::git(d.path(), &["add", "."]);
    common::git(d.path(), &["commit", "-q", "-m", "one"]);
    let head = common::git(d.path(), &["rev-parse", "HEAD"]);
    (d, head)
}

/// git pack-objects --revs → pack bytes
fn pack_of(dir: &std::path::Path, revs: &str) -> Vec<u8> {
    use std::process::{Command, Stdio};
    let mut c = Command::new("git")
        .args(["pack-objects", "--stdout", "--revs", "-q"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(revs.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_then_fetch() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    // --- advertise (empty repo)
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let adv = tokio::task::spawn_blocking(move || {
        let mut adv = Vec::new();
        receive::advertise(&s2, &repo2, &mut adv).map(|_| adv)
    })
    .await
    .unwrap()
    .unwrap();
    let adv = String::from_utf8_lossy(&adv).to_string();
    assert!(adv.contains("capabilities^{}"), "{adv}");
    assert!(adv.contains("report-status"), "{adv}");

    // --- push
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!(
            "{} {} refs/heads/main\0report-status side-band-64k",
            "0".repeat(40),
            head
        )
        .as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    // decode sideband band-1 payload
    let mut c = Cursor::new(resp);
    let mut payload = Vec::new();
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p {
            if d[0] == 1 {
                payload.extend_from_slice(&d[1..]);
            }
        }
    }
    let text = String::from_utf8_lossy(&payload).to_string();
    assert!(text.contains("unpack ok"), "{text}");
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert_eq!(
        s.get_ref(&repo, "refs/heads/main")
            .await
            .unwrap()
            .unwrap()
            .to_hex()
            .to_string(),
        head
    );
    // pack landed in S3 (re-open after wiping cache)
    std::fs::remove_dir_all(&repo.pack_dir).unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(std::fs::read_dir(&repo.pack_dir).unwrap().count() >= 2);

    // --- advertise now lists main
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let adv = tokio::task::spawn_blocking(move || {
        let mut adv = Vec::new();
        receive::advertise(&s2, &repo2, &mut adv).map(|_| adv)
    })
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&adv).contains(&format!("{head} refs/heads/main")));

    // --- delete-only push (no pack)
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {} refs/heads/main\0report-status", head, "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&resp).contains("ok refs/heads/main"));
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());

    // re-push main so we have something to fetch
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {} refs/heads/main\0report-status", "0".repeat(40), head).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
    })
    .await
    .unwrap()
    .unwrap();

    // ls-refs
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=ls-refs").unwrap();
    pktline::write_text(&mut req, "agent=git/2.x").unwrap();
    pktline::write_text(&mut req, "object-format=sha1").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "symrefs").unwrap();
    pktline::write_text(&mut req, "ref-prefix refs/heads/").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains(&format!("{head} refs/heads/main")), "{text}");

    // fetch with done -> packfile
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_text(&mut req, "agent=git/2.x").unwrap();
    pktline::write_text(&mut req, "object-format=sha1").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "no-progress").unwrap();
    pktline::write_text(&mut req, &format!("want {head}")).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    // parse: sections until "packfile", then band-1 data
    let mut c = Cursor::new(resp);
    let mut pack = Vec::new();
    let mut in_pack = false;
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p {
            if in_pack {
                if d[0] == 1 {
                    pack.extend_from_slice(&d[1..]);
                }
            } else if d == b"packfile\n" {
                in_pack = true;
            }
        }
    }
    assert!(pack.starts_with(b"PACK"));
    // verify with git index-pack in a scratch repo
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    let mut c = std::process::Command::new("git")
        .args(["index-pack", "--stdin"])
        .current_dir(scratch.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(&pack).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    common::git(scratch.path(), &["cat-file", "-e", &head]);

    // A want that is not a ref tip but IS reachable from one is served: this is
    // the promisor fetch a partial clone makes when it comes back for an object
    // it left behind. The boundary is reachability, not tip-ness — see
    // `an_object_no_ref_reaches_is_refused` for the other side of it.
    let tree = common::git(local.path(), &["rev-parse", "HEAD^{tree}"]);
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, &format!("want {tree}")).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let body = String::from_utf8_lossy(&resp);
    assert!(
        !body.contains("ERR upload-pack: not our ref"),
        "a reachable object is fetchable: {body:?}",
    );
    assert!(body.contains("packfile"), "and comes back as a pack: {body:?}");

    // fetch without done, with an unknown have -> acknowledgments + NAK, no packfile
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_text(&mut req, "agent=git/2.x").unwrap();
    pktline::write_text(&mut req, "object-format=sha1").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, &format!("want {head}")).unwrap();
    pktline::write_text(&mut req, &format!("have {}", "2".repeat(40))).unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("acknowledgments"), "{text}");
    assert!(text.contains("NAK"), "{text}");
    assert!(!text.contains("packfile"), "{text}");

    // wait-for-done: a known have is ACKed but the server must not go "ready"
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "wait-for-done").unwrap();
    pktline::write_text(&mut req, &format!("want {head}")).unwrap();
    pktline::write_text(&mut req, &format!("have {head}")).unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains(&format!("ACK {head}")), "{text}");
    assert!(!text.contains("ready"), "{text}");
    assert!(!text.contains("packfile"), "{text}");

    // annotated tag want: the pack must carry the tag and everything it peels to
    common::git(local.path(), &["tag", "-a", "v1", "-m", "v1"]);
    let tag = common::git(local.path(), &["rev-parse", "v1"]);
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {tag} refs/tags/v1\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{tag}\n")));
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, &format!("want {tag}")).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let mut c = Cursor::new(resp);
    let mut pack = Vec::new();
    let mut in_pack = false;
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p {
            if in_pack {
                if d[0] == 1 {
                    pack.extend_from_slice(&d[1..]);
                }
            } else if d == b"packfile\n" {
                in_pack = true;
            }
        }
    }
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    let mut c = std::process::Command::new("git")
        .args(["index-pack", "--stdin"])
        .current_dir(scratch.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(&pack).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    common::git(scratch.path(), &["cat-file", "-e", &tag]);
    common::git(scratch.path(), &["cat-file", "-e", &head]);
}

/// A batch where one command is stale must reject every ref and change nothing.
#[tokio::test(flavor = "multi_thread")]
async fn atomic_push_rejects_whole_batch() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    // first push establishes refs/heads/main
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {} refs/heads/main\0report-status", "0".repeat(40), head).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();

    // second push: unknown object, a valid new ref, and a stale update to main
    let bogus = "1".repeat(40);
    let mut req = Vec::new();
    for cmd in [
        format!("{} {bogus} refs/heads/missing", "0".repeat(40)),
        format!("{} {head} refs/heads/x", "0".repeat(40)),
        format!("{bogus} {head} refs/heads/main\0report-status"),
    ] {
        pktline::write_pkt(&mut req, cmd.as_bytes()).unwrap();
    }
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo2 = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("unpack ok"), "{text}");
    assert!(
        text.contains("ng refs/heads/missing missing necessary objects"),
        "{text}"
    );
    assert!(text.contains("ng refs/heads/main fetch first"), "{text}");
    assert!(
        text.contains("ng refs/heads/x atomic push failed"),
        "{text}"
    );

    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/x").await.unwrap().is_none());
    assert_eq!(
        s.get_ref(&repo, "refs/heads/main")
            .await
            .unwrap()
            .unwrap()
            .to_hex()
            .to_string(),
        head
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn head_falls_back_to_existing_branch() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let oid = gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    let ls_refs = |s: std::sync::Arc<rustic_git_storage::store::Store>| async move {
        let repo = s.open_repo("a", "r").await.unwrap().unwrap();
        let mut req = Vec::new();
        pktline::write_text(&mut req, "command=ls-refs").unwrap();
        pktline::write_delim(&mut req).unwrap();
        pktline::write_text(&mut req, "symrefs").unwrap();
        pktline::write_text(&mut req, "unborn").unwrap();
        pktline::write_flush(&mut req).unwrap();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            upload::serve(
                &s,
                &repo,
                &mut Cursor::new(req),
                &mut out,
                &Default::default(),
            )
            .map(|_| out)
        })
        .await
        .unwrap()
        .map(|o| String::from_utf8_lossy(&o).to_string())
        .unwrap()
    };
    // empty repo: unborn main
    assert!(ls_refs(s.clone())
        .await
        .contains("unborn HEAD symref-target:refs/heads/main"));
    // only master exists: HEAD -> master
    s.update_refs(
        &repo,
        &[rustic_git_gitbase::refs::RefUpdate {
            name: "refs/heads/master".into(),
            old: None,
            new: Some(oid),
        }],
    )
    .await
    .unwrap();
    assert!(ls_refs(s.clone())
        .await
        .contains("HEAD symref-target:refs/heads/master"));
    // main appears: HEAD -> main again
    s.update_refs(
        &repo,
        &[rustic_git_gitbase::refs::RefUpdate {
            name: "refs/heads/main".into(),
            old: None,
            new: Some(oid),
        }],
    )
    .await
    .unwrap();
    assert!(ls_refs(s.clone())
        .await
        .contains("HEAD symref-target:refs/heads/main"));
}

#[tokio::test(flavor = "multi_thread")]
async fn repack_consolidates_and_preserves_history() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, _) = local_repo();

    // two pushes = two packs in the network
    let push = |branch: String, head: String, revs: String, dir: std::path::PathBuf| {
        let s = s.clone();
        async move {
            let mut req = Vec::new();
            pktline::write_pkt(
                &mut req,
                format!(
                    "{} {} refs/heads/{branch}\0report-status",
                    "0".repeat(40),
                    head
                )
                .as_bytes(),
            )
            .unwrap();
            pktline::write_flush(&mut req).unwrap();
            req.extend(pack_of(&dir, &revs));
            let repo = s.open_repo("a", "r").await.unwrap().unwrap();
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                receive::serve(
                    &s,
                    &repo,
                    &mut Cursor::new(req),
                    &mut out,
                    &Default::default(),
                )
                .map(|_| out)
            })
            .await
            .unwrap()
            .unwrap();
        }
    };
    common::git(local.path(), &["checkout", "-qb", "b1"]);
    std::fs::write(local.path().join("b.txt"), "b\n").unwrap();
    common::git(local.path(), &["add", "."]);
    common::git(local.path(), &["commit", "-qm", "two"]);
    let h1 = common::git(local.path(), &["rev-parse", "main"]);
    let h2 = common::git(local.path(), &["rev-parse", "b1"]);
    push(
        "main".into(),
        h1.clone(),
        format!("{h1}\n"),
        local.path().to_path_buf(),
    )
    .await;
    push(
        "b1".into(),
        h2.clone(),
        format!("{h2}\n^{h1}\n"),
        local.path().to_path_buf(),
    )
    .await;

    let prefix = slatedb::object_store::path::Path::from("objects/a/r/pack");
    let count_packs = || {
        let os = s.os.clone();
        let prefix = prefix.clone();
        async move {
            use futures::TryStreamExt;
            let v: Vec<_> = os.list(Some(&prefix)).try_collect().await.unwrap();
            v.iter()
                .filter(|m| m.location.extension() == Some("pack"))
                .count()
        }
    };
    assert_eq!(count_packs().await, 2);

    let (before, after) = s.repack("a", "r").await.unwrap();
    assert_eq!((before, after), (2, 1));
    assert_eq!(count_packs().await, 1);

    // both branch tips still fetch, and the consolidated pack indexes cleanly
    for tip in [&h1, &h2] {
        let mut req = Vec::new();
        pktline::write_text(&mut req, "command=fetch").unwrap();
        pktline::write_delim(&mut req).unwrap();
        pktline::write_text(&mut req, "no-progress").unwrap();
        pktline::write_text(&mut req, &format!("want {tip}")).unwrap();
        pktline::write_text(&mut req, "done").unwrap();
        pktline::write_flush(&mut req).unwrap();
        let s2 = s.clone();
        let repo = s.open_repo("a", "r").await.unwrap().unwrap();
        let resp = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            upload::serve(
                &s2,
                &repo,
                &mut Cursor::new(req),
                &mut out,
                &Default::default(),
            )
            .map(|_| out)
        })
        .await
        .unwrap()
        .unwrap();
        let mut c = Cursor::new(resp);
        let mut pack = Vec::new();
        let mut in_pack = false;
        while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
            if let pktline::Pkt::Data(d) = p {
                if in_pack {
                    if d[0] == 1 {
                        pack.extend_from_slice(&d[1..]);
                    }
                } else if d == b"packfile\n" {
                    in_pack = true;
                }
            }
        }
        assert!(pack.starts_with(b"PACK"), "no pack for {tip}");
        let scratch = tempfile::tempdir().unwrap();
        common::git(scratch.path(), &["init", "-q"]);
        let mut ip = std::process::Command::new("git")
            .args(["index-pack", "--stdin"])
            .current_dir(scratch.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        ip.stdin.take().unwrap().write_all(&pack).unwrap();
        let out = ip.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "index-pack: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        common::git(scratch.path(), &["cat-file", "-e", tip]);
    }
}

/// A push whose connectivity check rejects every update (e.g. a ref pointed at an object nobody
/// has) must not leave the pack it uploaded before that check ran sitting in the store forever.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_push_leaves_no_pack_in_store() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    let prefix = slatedb::object_store::path::Path::from("objects/a/r/pack");
    let count_packs = || {
        let os = s.os.clone();
        let prefix = prefix.clone();
        async move {
            use futures::TryStreamExt;
            let v: Vec<_> = os.list(Some(&prefix)).try_collect().await.unwrap();
            v.iter()
                .filter(|m| m.location.extension() == Some("pack"))
                .count()
        }
    };
    assert_eq!(count_packs().await, 0);

    // real pack content (from local_repo's commit), but the only ref update points at an object
    // that is neither in the pack nor already known to this repo — the connectivity check must
    // reject the whole push, so nothing here ends up reachable.
    let bogus = "1".repeat(40);
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {bogus} refs/heads/missing\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(
        text.contains("ng refs/heads/missing missing necessary objects"),
        "{text}"
    );

    assert_eq!(count_packs().await, 0, "rejected push must leave no pack behind");
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/missing").await.unwrap().is_none());
}

/// A repo with no refs at all (e.g. it never had a successful push) has nothing reachable, so
/// repack must reclaim its packs rather than skipping consolidation forever.
#[tokio::test(flavor = "multi_thread")]
async fn repack_reclaims_packs_when_no_tips_remain() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    // establish then delete the only ref, leaving an unreachable pack behind with no tips
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{head} {} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());

    let prefix = slatedb::object_store::path::Path::from("objects/a/r/pack");
    let count_packs = || {
        let os = s.os.clone();
        let prefix = prefix.clone();
        async move {
            use futures::TryStreamExt;
            let v: Vec<_> = os.list(Some(&prefix)).try_collect().await.unwrap();
            v.iter()
                .filter(|m| m.location.extension() == Some("pack"))
                .count()
        }
    };
    assert_eq!(count_packs().await, 1, "the now-unreachable pack is still there");

    let (before, after) = s.repack("a", "r").await.unwrap();
    assert_eq!((before, after), (1, 0));
    assert_eq!(count_packs().await, 0);
}

/// A push fully rejected by branch protection (force-push to a no_force branch) reaches
/// update_refs and fails atomically there, not via the connectivity/isolation walk — a different
/// code path than `rejected_push_leaves_no_pack_in_store`, and the pack it uploaded must still be
/// cleaned up.
#[tokio::test(flavor = "multi_thread")]
async fn protection_rejected_push_leaves_no_pack_in_store() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    s.set_protection(
        "a",
        "r",
        &rustic_git_gitbase::refs::Protection {
            pattern: "main".into(),
            no_force: true,
            no_delete: true,
        },
    )
    .await
    .unwrap();
    let (local, head) = local_repo();

    let prefix = slatedb::object_store::path::Path::from("objects/a/r/pack");
    let count_packs = || {
        let os = s.os.clone();
        let prefix = prefix.clone();
        async move {
            use futures::TryStreamExt;
            let v: Vec<_> = os.list(Some(&prefix)).try_collect().await.unwrap();
            v.iter()
                .filter(|m| m.location.extension() == Some("pack"))
                .count()
        }
    };

    // first push establishes refs/heads/main (creation, not a rewrite — the rule allows it)
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count_packs().await, 1);

    // second push: amend the commit so the new tip is not a descendant of `head` — a
    // non-fast-forward move, which is what the no_force rule refuses.
    std::fs::write(local.path().join("other.txt"), "diverge\n").unwrap();
    common::git(local.path(), &["add", "."]);
    common::git(local.path(), &["commit", "-q", "--amend", "-m", "diverge"]);
    let head2 = common::git(local.path(), &["rev-parse", "HEAD"]);

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{head} {head2} refs/heads/main\0report-status").as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head2}\n^{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("protected"), "{text}");

    assert_eq!(
        count_packs().await,
        1,
        "the protection-rejected push's pack must not linger"
    );
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert_eq!(
        s.get_ref(&repo, "refs/heads/main")
            .await
            .unwrap()
            .unwrap()
            .to_hex()
            .to_string(),
        head,
        "main must still point at the first push's commit"
    );
}

/// Guard against over-deletion: a successful push must keep its pack.
#[tokio::test(flavor = "multi_thread")]
async fn successful_push_retains_its_pack() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    let prefix = slatedb::object_store::path::Path::from("objects/a/r/pack");
    let count_packs = || {
        let os = s.os.clone();
        let prefix = prefix.clone();
        async move {
            use futures::TryStreamExt;
            let v: Vec<_> = os.list(Some(&prefix)).try_collect().await.unwrap();
            v.iter()
                .filter(|m| m.location.extension() == Some("pack"))
                .count()
        }
    };
    assert_eq!(count_packs().await, 0);

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("ok refs/heads/main"), "{text}");
    assert_eq!(count_packs().await, 1, "a successful push must keep its pack");

    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert_eq!(
        s.get_ref(&repo, "refs/heads/main")
            .await
            .unwrap()
            .unwrap()
            .to_hex()
            .to_string(),
        head
    );
}

/// C1: a fork must not be able to make a sibling repo's object its own ref tip (and then clone it
/// back out). The victim's object exists in the shared network odb but is not in the pushed pack
/// and is not reachable from the attacker's refs.
#[tokio::test(flavor = "multi_thread")]
async fn cannot_claim_sibling_object_as_tip() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("victim", "r").await.unwrap();
    let (local, head) = local_repo();

    // victim pushes its history
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("victim", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();

    // attacker forks (shares the object pool), then deletes every ref so it reaches nothing
    let victim = s.open_repo("victim", "r").await.unwrap().unwrap();
    s.fork(&victim, "attacker", "f").await.unwrap();
    let fork = s.open_repo("attacker", "f").await.unwrap().unwrap();
    let victim_oid = s
        .get_ref(&victim, "refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    s.update_refs(
        &fork,
        &[rustic_git_gitbase::refs::RefUpdate {
            name: "refs/heads/main".into(),
            old: Some(victim_oid),
            new: None,
        }],
    )
    .await
    .unwrap();
    assert!(s.list_refs(&fork).await.unwrap().is_empty());

    // attacker pushes a ref pointing at the victim's object, with NO pack
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!(
            "{} {} refs/heads/stolen\0report-status",
            "0".repeat(40),
            victim_oid.to_hex()
        )
        .as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let fork2 = s.open_repo("attacker", "f").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &fork2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(
        text.contains("ng refs/heads/stolen"),
        "push should be rejected: {text}"
    );
    assert!(
        s.get_ref(&fork, "refs/heads/stolen")
            .await
            .unwrap()
            .is_none(),
        "victim's object must not become a ref of the fork"
    );

    // and a direct want for it is still refused
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, &format!("want {}", victim_oid.to_hex())).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let fork2 = s.open_repo("attacker", "f").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(
            &s2,
            &fork2,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&resp).contains("not our ref"));
}

/// A legitimate push that creates a branch at an existing commit sends no objects; it must still
/// be accepted (the reachable-from-our-refs path).
#[tokio::test(flavor = "multi_thread")]
async fn branch_at_existing_commit_without_pack_is_accepted() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();

    // second push: new branch at the same commit, no pack at all
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/copy\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("ok refs/heads/copy"), "{text}");
}

/// A pack that omits objects the pushed tip needs must be rejected, not turned into a ref whose
/// history is broken. Here the client sends only the tip commit, without its parent.
#[tokio::test(flavor = "multi_thread")]
async fn gappy_pack_is_rejected() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();

    // local repo with two commits
    let (local, _) = local_repo();
    std::fs::write(local.path().join("b.txt"), "b\n").unwrap();
    common::git(local.path(), &["add", "."]);
    common::git(local.path(), &["commit", "-qm", "second"]);
    let head = common::git(local.path(), &["rev-parse", "HEAD"]);
    let parent = common::git(local.path(), &["rev-parse", "HEAD~1"]);

    // pack containing ONLY the second commit's objects (parent excluded)
    let pack = pack_of(local.path(), &format!("{head}\n^{parent}\n"));
    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack);
    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(
            &s2,
            &repo,
            &mut Cursor::new(req),
            &mut out,
            &Default::default(),
        )
        .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(
        text.contains("ng refs/heads/main"),
        "gappy push should be rejected: {text}"
    );
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());
}


/// An object that exists but that no ref reaches is NOT fetchable.
///
/// Partial clone made non-tip wants legal, and this is the line it must not cross.
/// A force-push leaves its old commits in the pack files; if merely knowing an id
/// were enough to fetch one, anyone who saw a sha in a stale link could read code
/// the branch no longer has. The test forks a repo (which copies its objects) and
/// removes every ref, so the objects are present and reachable from nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_object_no_ref_reaches_is_refused() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("owner", "r").await.unwrap();
    let (local, head) = local_repo();

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone();
    let repo = s.open_repo("owner", "r").await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(&s2, &repo, &mut Cursor::new(req), &mut out, &Default::default()).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();

    // A copy with the objects but no refs — the shape a force-push leaves behind.
    let src = s.open_repo("owner", "r").await.unwrap().unwrap();
    s.fork(&src, "other", "f").await.unwrap();
    let fork = s.open_repo("other", "f").await.unwrap().unwrap();
    let oid = s.get_ref(&src, "refs/heads/main").await.unwrap().unwrap();
    s.update_refs(
        &fork,
        &[rustic_git_gitbase::refs::RefUpdate {
            name: "refs/heads/main".into(),
            old: Some(oid),
            new: None,
        }],
    )
    .await
    .unwrap();
    assert!(s.list_refs(&fork).await.unwrap().is_empty(), "no ref reaches anything");

    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, &format!("want {}", oid.to_hex())).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone();
    let fork2 = s.open_repo("other", "f").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(&s2, &fork2, &mut Cursor::new(req), &mut out, &Default::default()).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let body = String::from_utf8_lossy(&resp);
    assert!(body.contains("ERR upload-pack: not our ref"), "refused: {body:?}");
    assert!(!body.contains("packfile"), "and no pack is sent: {body:?}");
}

/// The merge strategies, through the real ref-update path.
///
/// Squash and merge-commit are only safe when the base is an ancestor of the head
/// — then the content to land IS the head's tree and no three-way merge exists to
/// get wrong. This asserts the SHAPE each strategy leaves behind, because that is
/// the whole difference between them.
#[tokio::test(flavor = "multi_thread")]
async fn squash_and_merge_commit_land_the_right_shape() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();

    for (strategy, want_parents) in [("squash", 1usize), ("merge", 2usize)] {
        let repo = common::push_built(&e, "alice", strategy, |c| {
            std::fs::write(c.join("f.txt"), "base\n").unwrap();
            common::git(c, &["add", "."]);
            common::git(c, &["commit", "-qm", "base"]);
            std::fs::write(c.join("f.txt"), "one\n").unwrap();
            common::git(c, &["commit", "-qam", "one"]);
            std::fs::write(c.join("f.txt"), "two\n").unwrap();
            common::git(c, &["commit", "-qam", "two"]);
        })
        .await;
        let odb = repo.odb().unwrap();
        let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
        // Two commits back is the "base" the branch left.
        let log = rustic_git_git::browse::log(&odb, head, 3).unwrap();
        let base: gix_hash::ObjectId = log[2].oid.parse().unwrap();

        let mut buf = Vec::new();
        let head_tree = gix_object::FindExt::find_commit(&odb, &head, &mut buf).unwrap().tree();
        let landed = rustic_git_gitbase::objects::write_commit(
            &s,
            &repo,
            rustic_git_gitbase::objects::NewCommit {
                tree: head_tree,
                parents: if strategy == "squash" { vec![base] } else { vec![base, head] },
                message: format!("{strategy} landing\n"),
                author_name: "kloudlite".into(),
                author_email: "noreply@kloudlite.io".into(),
                time: 1_700_000_000,
            },
        )
        .await
        .unwrap();

        let repo2 = s.open_repo("alice", strategy).await.unwrap().unwrap();
        let odb2 = repo2.odb().unwrap();
        let mut b2 = Vec::new();
        let c = gix_object::FindExt::find_commit(&odb2, &landed, &mut b2).unwrap();
        assert_eq!(c.parents().count(), want_parents, "{strategy}: parent count");
        assert_eq!(c.tree(), head_tree, "{strategy}: lands exactly the head's content");
        assert_eq!(
            c.parents().next().unwrap(),
            base,
            "{strategy}: first parent is the base it lands on",
        );

        // And it is a real commit as far as everything else is concerned.
        let moved = s
            .update_refs(&repo2, &[rustic_git_gitbase::refs::RefUpdate {
                name: "refs/heads/master".into(),
                old: Some(head),
                new: Some(landed),
            }])
            .await
            .unwrap();
        assert_eq!(moved, vec![None], "{strategy}: the base moves to it");

        // The difference between the two strategies, stated exactly: a merge keeps
        // the branch reachable as its SECOND parent, a squash does not keep it at
        // all. Asserted on the parents rather than on a log walk, because
        // `browse::log` follows first parents only — by design, and a merge's
        // second parent is legitimately absent from it.
        let second = c.parents().nth(1);
        assert_eq!(
            second,
            (strategy == "merge").then_some(head),
            "{strategy}: the branch is {} as a second parent",
            if strategy == "merge" { "kept" } else { "not kept" },
        );
    }
}

/// Drive `upload::serve` with one fetch command and return the raw pack bytes it streamed.
fn fetch_pack_bytes(
    s: &std::sync::Arc<rustic_git_storage::store::Store>,
    repo: &rustic_git_storage::store::Repo,
    lines: &[String],
) -> Vec<u8> {
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_text(&mut req, "object-format=sha1").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "no-progress").unwrap();
    for l in lines {
        pktline::write_text(&mut req, l).unwrap();
    }
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let mut out = Vec::new();
    upload::serve(s, repo, &mut Cursor::new(req), &mut out, &Default::default()).unwrap();
    let mut c = Cursor::new(out);
    let (mut pack, mut in_pack) = (Vec::new(), false);
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p {
            if in_pack {
                if d.first() == Some(&1) {
                    pack.extend_from_slice(&d[1..]);
                }
            } else if d == b"packfile\n" {
                in_pack = true;
            }
        }
    }
    assert!(pack.starts_with(b"PACK"), "no pack came back");
    pack
}

/// An incremental fetch must cost O(what changed), not O(repo). With the tree snapshot
/// expansion every `git fetch` re-sent every blob; this pins the pack to the delta.
#[tokio::test(flavor = "multi_thread")]
async fn incremental_fetch_sends_the_delta_not_the_snapshot() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "big", |c| {
        // 200 incompressible 4 KiB files, so the pack size reflects the blobs carried.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for i in 0..200 {
            let mut body = Vec::with_capacity(4096);
            while body.len() < 4096 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                body.extend_from_slice(&x.to_le_bytes());
            }
            std::fs::write(c.join(format!("f{i}.bin")), &body).unwrap();
        }
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "snapshot"]);
        std::fs::write(c.join("f0.bin"), b"tiny change\n").unwrap();
        common::git(c, &["commit", "-qam", "one file"]);
    })
    .await;
    let s = e.store.clone();
    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let parent = gix_object::FindExt::find_commit(&odb, &head, &mut Vec::new())
        .unwrap()
        .parents()
        .next()
        .unwrap();

    let (s2, r2) = (s.clone(), repo.clone());
    let full =
        tokio::task::spawn_blocking(move || fetch_pack_bytes(&s2, &r2, &[format!("want {head}")]))
            .await
            .unwrap();
    let (s2, r2) = (s.clone(), repo.clone());
    let incremental = tokio::task::spawn_blocking(move || {
        fetch_pack_bytes(&s2, &r2, &[format!("want {head}"), format!("have {parent}")])
    })
    .await
    .unwrap();

    assert!(full.len() > 700 * 1024, "fixture is big enough to measure: {}", full.len());
    assert!(
        incremental.len() * 20 < full.len(),
        "incremental {} bytes vs clone {} bytes: the snapshot was re-sent",
        incremental.len(),
        full.len()
    );

    // And the delta pack is complete: a client holding `parent` can index it and read HEAD.
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    for pack in [&full, &incremental] {
        let mut c = std::process::Command::new("git")
            .args(["index-pack", "--stdin"])
            .current_dir(scratch.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(pack).unwrap();
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    common::git(scratch.path(), &["cat-file", "-e", &head.to_hex().to_string()]);
    common::git(
        scratch.path(),
        &["fsck", "--no-progress", "--connectivity-only", &head.to_hex().to_string()],
    );
}

/// A filtered fetch sends its explicit object list as-is — but a tree wanted BY ID is a
/// promisor fetch for that tree, and git expands it whole. Sending the bare tree object
/// leaves the client with a pack that names children it did not get.
#[tokio::test(flavor = "multi_thread")]
async fn a_wanted_tree_still_carries_its_contents_under_a_filter() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "treewant", |c| {
        std::fs::create_dir(c.join("sub")).unwrap();
        std::fs::write(c.join("sub/a.txt"), "a\n").unwrap();
        std::fs::write(c.join("top.txt"), "t\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "one"]);
    })
    .await;
    let s = e.store.clone();
    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let tree = gix_object::FindExt::find_commit(&odb, &head, &mut Vec::new())
        .unwrap()
        .tree();

    let (s2, r2) = (s.clone(), repo.clone());
    let pack = tokio::task::spawn_blocking(move || {
        fetch_pack_bytes(
            &s2,
            &r2,
            &[format!("want {tree}"), "filter blob:none".into()],
        )
    })
    .await
    .unwrap();

    // PACK, version, then the object count: the root tree alone would be 1.
    let n = u32::from_be_bytes(pack[8..12].try_into().unwrap());
    assert!(n >= 3, "a wanted tree must bring its subtree and blobs; pack holds {n} objects");
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    let mut c = std::process::Command::new("git")
        .args(["index-pack", "--stdin"])
        .current_dir(scratch.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(&pack).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    common::git(scratch.path(), &["cat-file", "-e", &format!("{tree}^{{tree}}")]);
}

/// A push of many branches off one base walks their shared history once — each accepted tip is
/// added to the hide set for the next ref's walk. What this pins is that the batching cannot cost
/// a ref its acceptance: every branch here shares the base commit, and all of them must land.
#[tokio::test(flavor = "multi_thread")]
async fn push_of_many_branches_off_one_base_accepts_all() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let (local, base) = local_repo();

    // 20 branches, each one commit past the same base.
    let mut tips = Vec::new();
    for i in 0..20 {
        common::git(local.path(), &["checkout", "-q", "-B", &format!("b{i}"), &base]);
        std::fs::write(local.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        common::git(local.path(), &["add", "."]);
        common::git(local.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        tips.push(common::git(local.path(), &["rev-parse", "HEAD"]));
    }

    let zero = "0".repeat(40);
    let mut req = Vec::new();
    for (i, tip) in tips.iter().enumerate() {
        let line = format!("{zero} {tip} refs/heads/b{i}");
        let line = if i == 0 { format!("{line}\0report-status") } else { line };
        pktline::write_pkt(&mut req, line.as_bytes()).unwrap();
    }
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &tips.join("\n")));

    let s2 = s.clone();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(&s2, &repo, &mut Cursor::new(req), &mut out, &Default::default()).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(!text.contains("ng refs/"), "a branch was rejected: {text}");

    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    for (i, tip) in tips.iter().enumerate() {
        let got = s.get_ref(&repo, &format!("refs/heads/b{i}")).await.unwrap();
        assert_eq!(got.map(|o| o.to_string()).as_deref(), Some(tip.as_str()));
    }
}
