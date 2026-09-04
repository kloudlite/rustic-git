mod common;

/// Minimal raw HTTP GET; returns the whole response as text.
fn raw_get(port: u16, path: &str, auth: Option<&str>) -> String {
    use base64::Engine;
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let a = match auth {
        Some(t) => format!(
            "Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(format!("x:{t}"))
        ),
        None => String::new(),
    };
    write!(
        c,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{a}\r\n"
    )
    .unwrap();
    let mut s = Vec::new();
    c.read_to_end(&mut s).unwrap();
    String::from_utf8_lossy(&s).to_string()
}

/// Like `raw_get`, but lets a test add one extra request header (e.g. `Git-Protocol`).
fn raw_get_with(port: u16, path: &str, auth: Option<&str>, extra_header: &str) -> String {
    use base64::Engine;
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let a = match auth {
        Some(t) => format!(
            "Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(format!("x:{t}"))
        ),
        None => String::new(),
    };
    write!(
        c,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{a}{extra_header}\r\n\r\n"
    )
    .unwrap();
    let mut s = Vec::new();
    c.read_to_end(&mut s).unwrap();
    String::from_utf8_lossy(&s).to_string()
}

/// Catches: a `git-upload-pack` advertisement request without `Git-Protocol: version=2` used to
/// map straight to `internal()` (500 "internal error"), which looks like a server bug and trips
/// alerting for what is really a client sending protocol v0. It must now be a 400 with a body
/// that names the fix, and the same request WITH the v2 header must still succeed.
#[tokio::test(flavor = "multi_thread")]
async fn upload_pack_without_v2_header_is_a_client_error_not_500() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let refs = "/alice/proj.git/info/refs?service=git-upload-pack";

    // no Git-Protocol header at all -> v0 -> client error, not 500
    let r = raw_get(port, refs, Some(&token));
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
    assert!(
        r.contains("protocol v2") && r.contains("protocol.version=2"),
        "body should name the cause and the remedy: {r}"
    );

    // with the v2 header, the same request succeeds
    let r = raw_get_with(port, refs, Some(&token), "Git-Protocol: version=2\r\n");
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
}

/// Like `raw_get`, but POSTs a body and returns the response.
fn raw_post(port: u16, path: &str, auth: Option<&str>, content_type: &str, body: &[u8]) -> String {
    use base64::Engine;
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let a = match auth {
        Some(t) => format!(
            "Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(format!("x:{t}"))
        ),
        None => String::new(),
    };
    write!(
        c,
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{a}\r\n",
        body.len()
    )
    .unwrap();
    c.write_all(body).unwrap();
    let mut s = Vec::new();
    c.read_to_end(&mut s).unwrap();
    String::from_utf8_lossy(&s).to_string()
}

/// Catches: a truncated pkt-line body (a length header promising more bytes than the client
/// sent) surfaced as `std::io::Error` from `pktline::read_pkt`, which `respond_first` mapped to
/// `internal()` (500) — indistinguishable from a genuine server fault. It must be a 400: the
/// client sent a malformed/truncated request, not us.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_push_body_is_a_client_error_not_500() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;

    // "0010" claims a 16-byte pkt (12-byte payload after the 4-byte length header) but the body
    // ends right there — read_pkt hits EOF mid-payload.
    let r = raw_post(
        port,
        "/alice/proj.git/git-receive-pack",
        Some(&token),
        "application/x-git-receive-pack-request",
        b"0010",
    );
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
}

/// Catches: an unknown `service=` query value hit the same `internal()` 500 path as a genuine
/// server failure. Must be a 400: the client asked for a service we do not support, not something
/// broken on our end.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_service_is_a_client_error_not_500() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let r = raw_get(
        port,
        "/alice/proj.git/info/refs?service=git-nonsense",
        Some(&token),
    );
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_push_fetch() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/proj.git");

    // clone empty
    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "c1"]);
    let c1 = w.path().join("c1");
    std::fs::write(c1.join("f.txt"), "one\n").unwrap();
    common::git(&c1, &["add", "."]);
    common::git(&c1, &["commit", "-q", "-m", "one"]);
    common::git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    // second commit + push (incremental pack, thin)
    std::fs::write(c1.join("f.txt"), "one\ntwo\n").unwrap();
    common::git(&c1, &["commit", "-qam", "two"]);
    common::git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let head = common::git(&c1, &["rev-parse", "HEAD"]);

    // fresh clone sees both commits
    common::git(w.path(), &["clone", "-q", &url, "c2"]);
    let c2 = w.path().join("c2");
    assert_eq!(common::git(&c2, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(c2.join("f.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(common::git(&c2, &["log", "--oneline"]).lines().count(), 2);

    // c2 pushes a branch, c1 fetches (negotiation with haves)
    common::git(&c2, &["checkout", "-qb", "feat"]);
    std::fs::write(c2.join("g.txt"), "g\n").unwrap();
    common::git(&c2, &["add", "."]);
    common::git(&c2, &["commit", "-qm", "feat"]);
    common::git(&c2, &["push", "-q", "origin", "feat"]);
    common::git(&c1, &["fetch", "-q", "origin"]);
    assert_eq!(
        common::git(&c1, &["rev-parse", "origin/feat"]),
        common::git(&c2, &["rev-parse", "HEAD"])
    );

    // delete branch, force push
    common::git(&c2, &["push", "-q", "origin", "--delete", "feat"]);
    common::git(&c1, &["fetch", "-q", "--prune", "origin"]);
    assert!(!std::process::Command::new("git")
        .args(["rev-parse", "-q", "--verify", "origin/feat"])
        .current_dir(&c1)
        .output()
        .unwrap()
        .status
        .success());
    common::git(&c1, &["reset", "-q", "--hard", "HEAD~1"]);
    common::git(&c1, &["push", "-q", "-f", "origin", "HEAD:refs/heads/main"]);
    common::git(&c2, &["fetch", "-q", "origin"]);
    assert_eq!(
        common::git(&c2, &["rev-parse", "origin/main"]),
        common::git(&c1, &["rev-parse", "HEAD"])
    );

    // auth: wrong token → fails; other owner → fails
    let bad = format!("http://x:nope@127.0.0.1:{port}/alice/proj.git");
    assert!(!std::process::Command::new("git")
        .args(["ls-remote", &bad])
        .output()
        .unwrap()
        .status
        .success());
    let bob = s.create_token("bob").await.unwrap();
    let bobu = format!("http://x:{bob}@127.0.0.1:{port}/alice/proj.git");
    assert!(!std::process::Command::new("git")
        .args(["ls-remote", &bobu])
        .output()
        .unwrap()
        .status
        .success());

    // direct auth-contract checks (status codes, not just git exit status)
    let refs = "/alice/proj.git/info/refs?service=git-upload-pack";
    let r = raw_get(port, refs, None);
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    assert!(
        r.to_lowercase()
            .contains("www-authenticate: basic realm=\"kloudlite-git\""),
        "{r}"
    );
    let r = raw_get(port, refs, Some(&bob));
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    let r = raw_get(
        port,
        "/alice/nonexistent.git/info/refs?service=git-upload-pack",
        Some(&token),
    );
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
}

/// Catches: `open` reading visibility through `db_for` before it knows the repo exists, which lets
/// an anonymous request on the public listener create and warm a SlateDB per distinct path.
#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_request_for_an_unknown_repo_opens_nothing() {
    let e = common::env().await;
    let s = e.store.clone();
    let port = common::serve(common::app(s.clone()).await).await;
    for p in [
        "/anyone/anything/info/refs?service=git-upload-pack",
        "/anyone/anything.git/info/refs?service=git-upload-pack",
        "/other/name.git/info/refs?service=git-upload-pack",
    ] {
        let r = raw_get(port, p, None);
        assert!(r.starts_with("HTTP/1.1 401"), "{p}: {r}");
    }
    assert_eq!(
        s.pool.warm_count(),
        0,
        "an unauthenticated stranger must not conjure a database"
    );
}

/// The existence gate must not become an oracle. Routing runs before authentication, so a missing
/// name that answered 404 there would tell an anonymous client which private repos exist. It is
/// served locally instead: the handler authenticates first, and only a caller who is allowed to
/// know gets the 404. Nothing is opened either way — that is the other half of the gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_repo_is_indistinguishable_from_a_private_one_until_you_authenticate() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "secret").await.unwrap(); // private by default
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;

    let warm = s.pool.warm_count(); // `create_repo` opened `secret`; nothing else may be opened
    let private = raw_get(port, "/alice/secret/info/refs?service=git-upload-pack", None);
    let missing = raw_get(port, "/alice/nosuch/info/refs?service=git-upload-pack", None);
    assert!(private.starts_with("HTTP/1.1 401"), "{private}");
    assert_eq!(missing, private, "a missing name must answer byte-for-byte as a private one does");

    // With credentials the answer may differ — that caller is allowed to know.
    let r = raw_get(port, "/alice/nosuch/info/refs?service=git-upload-pack", Some(&token));
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    assert_eq!(s.pool.warm_count(), warm, "no database was opened for a name that does not exist");
}

/// Shallow clone, through a real git client.
///
/// The failure this guards against is not "the clone errors" — it is a clone that
/// looks fine and is quietly broken, where `git log` walks off the boundary into
/// an object that was never sent. So every assertion is followed by `fsck` and a
/// full log, which is what would actually catch that.
#[tokio::test(flavor = "multi_thread")]
async fn shallow_clone_deepen_and_unshallow() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "deep").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/deep.git");

    // Five commits on one line of history.
    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    for i in 1..=5 {
        std::fs::write(src.join("f.txt"), format!("{i}\n")).unwrap();
        common::git(&src, &["add", "."]);
        common::git(&src, &["commit", "-qm", &format!("commit {i}")]);
    }
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let count = |dir: &std::path::Path| {
        common::git(dir, &["rev-list", "--count", "HEAD"]).trim().parse::<usize>().unwrap()
    };
    // `fsck` is the point: it fails loudly if the pack references an object the
    // server withheld, which a plain clone would not notice.
    let healthy = |dir: &std::path::Path| {
        common::git(dir, &["fsck", "--no-progress"]);
        common::git(dir, &["log", "--oneline"]);
    };

    // depth 1: the tip alone.
    common::git(w.path(), &["clone", "-q", "--depth", "1", &url, "d1"]);
    let d1 = w.path().join("d1");
    assert_eq!(count(&d1), 1, "depth 1 sends exactly the tip");
    assert!(d1.join(".git/shallow").exists(), "and records the boundary");
    healthy(&d1);

    // depth 3: three commits, still shallow.
    common::git(w.path(), &["clone", "-q", "--depth", "3", &url, "d3"]);
    let d3 = w.path().join("d3");
    assert_eq!(count(&d3), 3, "depth 3 sends three commits");
    healthy(&d3);

    // Deepening an existing shallow clone: the client re-sends its boundary and
    // gets the next commits, not a fresh copy of what it has.
    common::git(&d1, &["fetch", "-q", "--depth", "3", "origin"]);
    common::git(&d1, &["checkout", "-q", "-B", "main", "origin/main"]);
    assert_eq!(count(&d1), 3, "deepened to three");
    healthy(&d1);

    // --unshallow completes the history and drops the boundary entirely.
    common::git(&d1, &["fetch", "-q", "--unshallow", "origin"]);
    assert_eq!(count(&d1), 5, "unshallow brings the whole history");
    assert!(!d1.join(".git/shallow").exists(), "and the boundary file is gone");
    healthy(&d1);

    // A full clone is unaffected and never claims to be shallow.
    common::git(w.path(), &["clone", "-q", &url, "full"]);
    let full = w.path().join("full");
    assert_eq!(count(&full), 5);
    assert!(!full.join(".git/shallow").exists(), "a full clone is not shallow");
    healthy(&full);
}

/// Depth across a merge, and the two other ways to ask for less history.
///
/// A merge is where a depth-first walk gets this wrong: it follows one parent to
/// the bottom and cuts the other short, so the clone is missing objects on the
/// side it never visited. Depth has to be breadth-first for that reason.
#[tokio::test(flavor = "multi_thread")]
async fn depth_across_a_merge_and_the_other_cutoffs() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "fork").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/fork.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    let commit = |name: &str, file: &str| {
        std::fs::write(src.join(file), format!("{name}\n")).unwrap();
        common::git(&src, &["add", "."]);
        common::git(&src, &["commit", "-qm", name]);
    };
    commit("root", "root.txt");
    common::git(&src, &["branch", "-M", "main"]);
    common::git(&src, &["checkout", "-qb", "side"]);
    commit("side work", "side.txt");
    common::git(&src, &["checkout", "-q", "main"]);
    commit("main work", "main.txt");
    common::git(&src, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    // Both refs: `deepen-not` names a ref on the SERVER, so one that was never
    // pushed cannot be excluded.
    common::git(&src, &["push", "-q", "origin", "main", "side"]);

    // Depth 2 from the merge reaches BOTH parents, not one branch twice as deep.
    common::git(w.path(), &["clone", "-q", "--depth", "2", "--branch", "main", &url, "m2"]);
    let m2 = w.path().join("m2");
    common::git(&m2, &["fsck", "--no-progress"]);
    let subjects = common::git(&m2, &["log", "--format=%s"]);
    assert!(subjects.contains("merge side"), "the tip: {subjects}");
    assert!(subjects.contains("main work"), "one parent: {subjects}");
    assert!(subjects.contains("side work"), "and the other: {subjects}");
    assert!(!subjects.contains("root"), "but not past the boundary: {subjects}");

    // deepen-not: cut where a named ref already is.
    common::git(w.path(), &["clone", "-q", "--shallow-exclude", "side", "--branch", "main", &url, "dn"]);
    let dn = w.path().join("dn");
    common::git(&dn, &["fsck", "--no-progress"]);
    assert!(
        !common::git(&dn, &["log", "--format=%s"]).contains("side work"),
        "everything at and below `side` is excluded",
    );

    // deepen-since: nothing older than the cutoff. These commits are all "now",
    // so a cutoff in the future must leave only the boundary itself.
    let future = format!("@{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600);
    common::git(w.path(), &["clone", "-q", "--shallow-since", &future, "--branch", "main", &url, "ds"]);
    let ds = w.path().join("ds");
    common::git(&ds, &["fsck", "--no-progress"]);
    assert_eq!(
        common::git(&ds, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "a cutoff after every commit leaves just the tip",
    );
}

/// `deepen-since` names the youngest commit >= the cutoff as the boundary —
/// never the too-old commit itself.
///
/// Regression for an off-by-one: the too-old commit used to be inserted into
/// the pack (and reported as the shallow point) before the cutoff check ran.
/// Three commits with controlled dates, cutoff strictly between the first two,
/// so the middle commit is the boundary and the oldest is excluded entirely.
#[tokio::test(flavor = "multi_thread")]
async fn deepen_since_excludes_the_too_old_commit_itself() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "since").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/since.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    // Base time far enough back that "base + 1 day" cutoffs land cleanly
    // between commits, with a full day between each commit's date.
    let base: i64 = 1_700_000_000;
    let commit_at = |name: &str, secs: i64| {
        std::fs::write(src.join("f.txt"), format!("{name}\n")).unwrap();
        common::git(&src, &["add", "."]);
        let date = format!("{secs} +0000");
        // Bypasses `common::git` only to pin the dates; it must still carry the identity that
        // helper sets, or a runner with no global user.email commits nothing and the later
        // push fails on a missing HEAD.
        let st = std::process::Command::new("git")
            .current_dir(&src)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .args(["-c", "commit.gpgsign=false", "commit", "-qm", name])
            .status()
            .unwrap();
        assert!(st.success(), "commit {name} failed");
    };
    commit_at("old", base); // too old — must be excluded entirely
    commit_at("boundary", base + 86_400); // the shallow boundary itself
    commit_at("tip", base + 2 * 86_400);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    // Cutoff strictly between "old" and "boundary".
    let cutoff = format!("@{}", base + 43_200);
    common::git(w.path(), &["clone", "-q", "--shallow-since", &cutoff, "--branch", "main", &url, "ds"]);
    let ds = w.path().join("ds");
    common::git(&ds, &["fsck", "--no-progress"]);
    let subjects = common::git(&ds, &["log", "--format=%s"]);
    assert!(subjects.contains("tip"), "tip present: {subjects}");
    assert!(subjects.contains("boundary"), "youngest commit >= cutoff present: {subjects}");
    assert!(subjects.lines().all(|l| l != "old"), "the too-old commit must not be sent: {subjects}");
    assert_eq!(
        common::git(&ds, &["rev-list", "--count", "HEAD"]).trim(),
        "2",
        "exactly boundary + tip, not the too-old commit: {subjects}",
    );
}

/// A cutoff the server cannot resolve is refused, not ignored.
///
/// The tempting behaviour is to shrug and send everything. That turns "give me a
/// small clone" into a full transfer with only a warning — the exact failure mode
/// `--filter` has today, and not one worth reproducing on purpose.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_cutoff_is_refused_rather_than_ignored() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "cut").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/cut.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    std::fs::write(src.join("f.txt"), "one\n").unwrap();
    common::git(&src, &["add", "."]);
    common::git(&src, &["commit", "-qm", "one"]);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let out = std::process::Command::new("git")
        .current_dir(w.path())
        .args(["-c", "commit.gpgsign=false"])
        .args(["clone", "--shallow-exclude", "no-such-branch", "--branch", "main", &url, "bad"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the clone must fail");
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("no such ref"), "and say why: {msg}");
    assert!(!w.path().join("bad/.git").exists(), "nothing half-cloned is left behind");
}

/// The compatibility batch, each through a real client: tags arrive with a clone,
/// `ls-remote` peels annotated tags, and an atomic push is all-or-nothing.
#[tokio::test(flavor = "multi_thread")]
async fn tags_peeling_and_atomic_push() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "tagged").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/tagged.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    std::fs::write(src.join("f.txt"), "one\n").unwrap();
    common::git(&src, &["add", "."]);
    common::git(&src, &["commit", "-qm", "one"]);
    common::git(&src, &["tag", "light"]);
    common::git(&src, &["tag", "-a", "v1", "-m", "release one"]);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main", "--tags"]);

    // include-tag: a plain clone brings the tags along.
    common::git(w.path(), &["clone", "-q", &url, "c"]);
    let c = w.path().join("c");
    let tags = common::git(&c, &["tag"]);
    assert!(tags.contains("v1"), "annotated tag came with the clone: {tags:?}");
    assert!(tags.contains("light"), "lightweight tag too: {tags:?}");
    common::git(&c, &["fsck", "--no-progress"]);

    // peel: ls-remote shows what the annotated tag points at.
    let remote = common::git(&c, &["ls-remote", &url]);
    assert!(remote.contains("refs/tags/v1^{}"), "annotated tag is peeled: {remote}");

    // atomic: one bad ref and NOTHING lands.
    std::fs::write(src.join("f.txt"), "two\n").unwrap();
    common::git(&src, &["commit", "-qam", "two"]);
    let before = common::git(&src, &["rev-parse", "origin/main"]);
    let out = std::process::Command::new("git")
        .current_dir(&src)
        // The second update is a non-fast-forward onto an existing tag, which the
        // server refuses; with --atomic the first must not land either.
        .args(["push", "--atomic", &url, "HEAD:refs/heads/main", "HEAD:refs/tags/v1"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the push is refused");
    let after = common::git(&src, &["ls-remote", &url, "refs/heads/main"]);
    assert!(
        after.contains(before.trim()),
        "main did not move: was {before:?}, remote now {after:?}",
    );
}

/// Partial clone: history whole, file contents fetched on demand.
///
/// The clone succeeding proves little — the real question is whether the client
/// can come BACK for a blob it left behind. That second fetch is the part the
/// server used to refuse, and a clone that breaks the first time you open a file
/// is worse than no partial clone at all.
#[tokio::test(flavor = "multi_thread")]
async fn partial_clone_fetches_blobs_on_demand() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "lazy").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/lazy.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    for i in 1..=3 {
        std::fs::write(src.join(format!("f{i}.txt")), format!("contents of {i}\n")).unwrap();
        common::git(&src, &["add", "."]);
        common::git(&src, &["commit", "-qm", &format!("commit {i}")]);
    }
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    // blob:none — all three commits, no file contents yet.
    common::git(w.path(), &["clone", "-q", "--filter=blob:none", &url, "none"]);
    let none = w.path().join("none");
    assert_eq!(
        common::git(&none, &["rev-list", "--count", "HEAD"]).trim(),
        "3",
        "history is complete",
    );
    // The pack must actually LACK the blobs — `--missing=print` lists them without fetching.
    // A server that expands the filtered list back to whole trees passes every other check
    // here. Checked on a --no-checkout clone: checking out HEAD lazily fetches every blob in
    // it, so the clone above cannot show the difference.
    common::git(w.path(), &["clone", "-q", "--filter=blob:none", "--no-checkout", &url, "nocheckout"]);
    let missing = common::git(
        &w.path().join("nocheckout"),
        &["rev-list", "--objects", "--missing=print", "HEAD"],
    );
    assert!(
        missing.lines().filter(|l| l.starts_with('?')).count() >= 3,
        "blob:none must leave the blobs behind; got:\n{missing}"
    );
    // The promisor fetch: reading a file makes the client go back for that blob.
    // If the server refuses an object-id want, this is where it breaks.
    assert_eq!(
        std::fs::read_to_string(none.join("f2.txt")).unwrap(),
        "contents of 2\n",
        "a checked-out file still has its contents",
    );
    let old = common::git(&none, &["show", "-q", "--format=%s", "HEAD~2"]);
    assert!(old.contains("commit 1"), "and history is readable: {old}");
    common::git(&none, &["fsck", "--no-progress"]);

    // tree:0 — commits only, the leanest form.
    common::git(w.path(), &["clone", "-q", "--filter=tree:0", "--no-checkout", &url, "t0"]);
    let t0 = w.path().join("t0");
    assert_eq!(common::git(&t0, &["rev-list", "--count", "HEAD"]).trim(), "3");

    // blob:limit — small files travel with the clone, large ones do not.
    common::git(w.path(), &["clone", "-q", "--filter=blob:limit=1k", &url, "lim"]);
    let lim = w.path().join("lim");
    assert_eq!(
        std::fs::read_to_string(lim.join("f1.txt")).unwrap(),
        "contents of 1\n",
        "a file under the limit is present",
    );
    common::git(&lim, &["fsck", "--no-progress"]);

    // A filter we do not implement is refused rather than silently ignored.
    let bad = std::process::Command::new("git")
        .current_dir(w.path())
        .args(["-c", "commit.gpgsign=false"])
        .args(["clone", "--filter=sparse:oid=deadbeef", &url, "bad"])
        .output()
        .unwrap();
    assert!(!bad.status.success(), "an unsupported filter fails");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("not supported"),
        "and says so: {}",
        String::from_utf8_lossy(&bad.stderr),
    );
}

/// `git push -o` is accepted, and the v2 status report is understood.
///
/// push-options matter even before anything reads them: they arrive between the
/// commands and the pack, so a server that does not consume them reads option
/// text as pack bytes and the push fails for a reason nobody could guess.
#[tokio::test(flavor = "multi_thread")]
async fn push_options_and_status_v2() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "opts").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/opts.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    std::fs::write(src.join("f.txt"), "one\n").unwrap();
    common::git(&src, &["add", "."]);
    common::git(&src, &["commit", "-qm", "one"]);

    // Two options, and a push that must still land.
    common::git(&src, &[
        "push", "-q", "-o", "ci.skip=true", "-o", "reason=testing",
        "origin", "HEAD:refs/heads/main",
    ]);
    let remote = common::git(&src, &["ls-remote", &url, "refs/heads/main"]);
    let head = common::git(&src, &["rev-parse", "HEAD"]);
    assert!(remote.contains(head.trim()), "the push landed: {remote}");

    // A second push with options, this time a rejection: the report has to come
    // back cleanly rather than the connection breaking. Move the remote on first,
    // so pushing the older commit is a genuine rewind rather than a no-op.
    std::fs::write(src.join("f.txt"), "two\n").unwrap();
    common::git(&src, &["commit", "-qam", "two"]);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let out = std::process::Command::new("git")
        .current_dir(&src)
        .args(["-c", "commit.gpgsign=false"])
        .args(["push", "-o", "x=1", &url, "HEAD~1:refs/heads/main"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a non-fast-forward is refused");
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("rejected") || msg.contains("fetch first"), "with a reason: {msg}");
}

/// A merge commit whose tree equals one of its parents' — exactly what the server-side PR merge
/// writes — must still clone. Both parent orders, because the pack counter's per-parent handling
/// is order-sensitive.
async fn clone_merge_with_parent_tree(feat_is_first_parent: bool) {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/proj.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    std::fs::write(src.join("base.txt"), "base\n").unwrap();
    common::git(&src, &["add", "."]);
    common::git(&src, &["commit", "-qm", "base"]);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    common::git(&src, &["checkout", "-qb", "feat"]);
    std::fs::write(src.join("feat.txt"), "feat\n").unwrap();
    common::git(&src, &["add", "."]);
    common::git(&src, &["commit", "-qm", "feat"]);
    common::git(&src, &["push", "-q", "origin", "feat"]);

    // Same shape as src/http/browse_api/merge.rs: two parents, tree taken verbatim from one of
    // them (so the merge itself introduces no change against that parent).
    let (p1, p2) = if feat_is_first_parent { ("feat", "main") } else { ("main", "feat") };
    let tree = common::git(&src, &["rev-parse", "feat^{tree}"]);
    let merge = common::git(&src, &["commit-tree", &tree, "-p", p1, "-p", p2, "-m", "merge"]);
    common::git(&src, &["push", "-q", "origin", &format!("{merge}:refs/heads/main")]);

    // The bug: `fetch-pack: invalid index-pack output` / "did not receive expected object".
    common::git(w.path(), &["clone", "-q", &url, "fresh"]);
    let fresh = w.path().join("fresh");
    assert_eq!(common::git(&fresh, &["rev-parse", "origin/main"]), merge);
    common::git(&fresh, &["cat-file", "-e", "origin/feat:feat.txt"]);
    common::git(&fresh, &["fsck", "--no-progress"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_merge_tree_equals_first_parent() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    clone_merge_with_parent_tree(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_merge_tree_equals_last_parent() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    clone_merge_with_parent_tree(false).await;
}

/// Every push adds a pack; the owner's lane folds them back into one once there are more than
/// the threshold. The fold must lose nothing: a fresh clone still `fsck`s clean afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn lane_consolidates_packs_past_threshold() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "many").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let app = common::app(s.clone()).await;
    let port = common::serve(app.clone()).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/many.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    let packs = || async {
        s.pack_index("alice", "many").await.unwrap().iter().filter(|(f, _)| f.ends_with(".pack")).count()
    };
    for i in 1..=4 {
        std::fs::write(src.join("f.txt"), format!("{i}\n")).unwrap();
        common::git(&src, &["add", "."]);
        common::git(&src, &["commit", "-qm", &format!("commit {i}")]);
        common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }
    assert_eq!(packs().await, 4);

    // at the threshold: untouched; past it: one pack
    kloudlite_git_server::lanes::consolidate_owned_packs(&app, 4).await;
    assert_eq!(packs().await, 4);
    kloudlite_git_server::lanes::consolidate_owned_packs(&app, 3).await;
    assert_eq!(packs().await, 1);

    common::git(w.path(), &["clone", "-q", &url, "again"]);
    let again = w.path().join("again");
    common::git(&again, &["fsck", "--no-progress"]);
    assert_eq!(
        common::git(&again, &["rev-parse", "HEAD"]),
        common::git(&src, &["rev-parse", "HEAD"])
    );
    // and the repo still takes pushes afterwards
    std::fs::write(src.join("f.txt"), "after\n").unwrap();
    common::git(&src, &["commit", "-qam", "after"]);
    common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    assert_eq!(packs().await, 2);
}

/// A consolidation that dies after the new pack is recorded but before the old ones go leaves
/// both indexed — duplicates, never a hole — and the next run finishes the job.
#[tokio::test(flavor = "multi_thread")]
async fn consolidation_crash_before_retire_leaves_duplicates_not_holes() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    use kloudlite_git_server::gc::RepackExt;
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "crash").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/crash.git");

    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "src"]);
    let src = w.path().join("src");
    for i in 1..=3 {
        std::fs::write(src.join("f.txt"), format!("{i}\n")).unwrap();
        common::git(&src, &["add", "."]);
        common::git(&src, &["commit", "-qm", &format!("commit {i}")]);
        common::git(&src, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }
    let packs = || async {
        s.pack_index("alice", "crash").await.unwrap().iter().filter(|(f, _)| f.ends_with(".pack")).count()
    };
    assert_eq!(packs().await, 3);

    // the "crash": rebuild ran, retire never did
    let repo = s.open_repo("alice", "crash").await.unwrap().unwrap();
    let old = kloudlite_git_server::gc::rebuild(&s, &repo, false).await.unwrap().unwrap();
    assert_eq!(old.iter().filter(|(f, _)| f.ends_with(".pack")).count(), 3);
    assert_eq!(packs().await, 4);
    // a cold cache is what a restart sees: every indexed pack must still be fetchable
    std::fs::remove_dir_all(&repo.pack_dir).unwrap();
    s.open_repo("alice", "crash").await.unwrap().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "mid"]);
    common::git(&w.path().join("mid"), &["fsck", "--no-progress"]);

    // the next run converges
    assert_eq!(s.consolidate("alice", "crash").await.unwrap(), (4, 1));
    common::git(w.path(), &["clone", "-q", &url, "after"]);
    common::git(&w.path().join("after"), &["fsck", "--no-progress"]);
}

/// Like `raw_get`, but with a username of the test's choosing — every other helper here sends
/// git's `x` placeholder, which is exactly the half this test has to vary.
fn raw_get_as(port: u16, path: &str, user: &str, token: &str) -> String {
    use base64::Engine;
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
    write!(
        c,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Authorization: Basic {cred}\r\nGit-Protocol: version=2\r\n\r\n"
    )
    .unwrap();
    let mut s = Vec::new();
    c.read_to_end(&mut s).unwrap();
    String::from_utf8_lossy(&s).to_string()
}

/// The token is the secret, but the username must name the owner it belongs to (or be git's `x`).
/// Halves that disagree did not verify: the answer is 401, never a silent fall-through to
/// anonymous — which on a PUBLIC repo would look like a success and hide a wrong credential.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_token_under_the_wrong_username_is_refused() {
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = common::serve(common::app(s.clone()).await).await;
    let refs = "/alice/proj.git/info/refs?service=git-upload-pack";

    // The matching halves work, so the refusals below are about the mismatch and nothing else.
    assert!(raw_get_as(port, refs, "x", &token).starts_with("HTTP/1.1 200"));
    assert!(raw_get_as(port, refs, "alice", &token).starts_with("HTTP/1.1 200"));

    // A real token, a username that is neither `x` nor its owner: 401.
    let r = raw_get_as(port, refs, "bob", &token);
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");

    // And on a PUBLIC repo it is still 401, not a fall-through to the anonymous read.
    s.set_public("alice", "proj", true).await.unwrap();
    let r = raw_get_as(port, refs, "bob", &token);
    assert!(r.starts_with("HTTP/1.1 401"), "a wrong username must not read anonymously: {r}");
}
