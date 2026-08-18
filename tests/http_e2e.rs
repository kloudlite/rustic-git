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
            .contains("www-authenticate: basic realm=\"rustic-git\""),
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
