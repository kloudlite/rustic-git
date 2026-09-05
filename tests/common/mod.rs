#![allow(dead_code)]
use kloudlite_storage::store::Store;

/// git pack-objects --revs → pack bytes
pub fn pack_of(dir: &std::path::Path, revs: &str) -> Vec<u8> {
    use std::io::Write;
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
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;

pub struct TestEnv {
    pub store: Arc<Store>,
    pub _tmp: tempfile::TempDir,
}

pub async fn env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let os = Arc::new(InMemory::new());
    let mut store = Store::open(os.clone(), tmp.path().join("cache"), false).await.unwrap();
    // `InMemory` implements `MultipartStore`, so the default test store exercises the chunked
    // upload fast path — the same one S3 gets. `env_file` is the fallback's harness.
    store.mp = Some(os);
    TestEnv {
        store: Arc::new(store),
        _tmp: tmp,
    }
}

/// Like `env`, over a real directory and with NO multipart store — `LocalFileSystem` is what
/// `KLOUDLITE_S3_URL=file://` gives, and object_store has no `MultipartStore` for it. The harness
/// for proving the chunked-upload fallback still works.
pub async fn env_file() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let os = slatedb::object_store::local::LocalFileSystem::new_with_prefix(tmp.path()).unwrap();
    let store = Store::open(Arc::new(os), tmp.path().join("cache"), false).await.unwrap();
    TestEnv { store: Arc::new(store), _tmp: tmp }
}

pub async fn serve_public_file() -> (String, TestEnv) {
    let e = env_file().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, kloudlite_server::router::router(app)).await.unwrap();
    });
    (base, e)
}

/// Like `env`, but with an in-process cache instead of a disabled one: the only way a test can
/// observe that a write path invalidated anything without a live Redis.
pub async fn env_cached() -> TestEnv {
    let e = env().await;
    let mut store = Arc::try_unwrap(e.store).ok().unwrap();
    store.cache = Arc::new(kloudlite_storage::cache::Cache::memory());
    TestEnv { store: Arc::new(store), _tmp: e._tmp }
}

/// An App for tests that are not about routing: this node takes the lease on its first beat, so
/// it is the leader and every claim is decided locally against its own ownership database.
pub async fn app(store: Arc<Store>) -> Arc<kloudlite_app::App> {
    let ownership = kloudlite_storage::ownership::OwnershipStore::open(store.os.clone());
    let app = kloudlite_app::App::new(
        store,
        Arc::new(ownership),
        "kloudlite-0".into(),
        // Nothing is ever forwarded here: this node owns whatever it claims.
        Arc::new(|_| "127.0.0.1:1".to_string()),
        "test-peer-secret".into(),
        kloudlite_pulls::pulls::Source::Absent,
    );
    // One beat: with nobody else on this store the node takes the lease and every claim is local.
    app.election_tick().await.unwrap();
    assert!(app.is_leader());
    Arc::new(app)
}

pub fn have_git() -> bool {
    have_tool("git", "--version")
}

pub fn have_ssh() -> bool {
    have_tool("ssh", "-V")
}

/// Every git/ssh-dependent test skips itself when the tool is absent — fine on a laptop, and a
/// silent green on a CI runner missing the tool. CI sets `KLOUDLITE_REQUIRE_GIT=1`, which turns
/// the skip into a failure.
fn have_tool(bin: &str, probe: &str) -> bool {
    let ok = std::process::Command::new(bin).arg(probe).output().map(|o| o.status.success()).unwrap_or(false);
    assert!(
        ok || std::env::var_os("KLOUDLITE_REQUIRE_GIT").is_none(),
        "{bin} is not installed and KLOUDLITE_REQUIRE_GIT is set: this test must not skip here"
    );
    ok
}

/// Run git in `dir`, panic on failure, return stdout.
pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        // Hermetic against whoever is running this. A developer with
        // `commit.gpgsign = true` would otherwise have every fixture commit try
        // to reach a passphrase prompt that does not exist here, and the suite
        // fails for a reason that has nothing to do with the code.
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        // Never let git ask a human anything. A prompt here has no terminal to
        // draw on and no one to answer it, so the subprocess blocks forever and
        // the suite hangs with no output -- which reads as a deadlock in the code
        // under test rather than as a credential git could not find.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GCM_INTERACTIVE", "never")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The fingerprint of an OpenSSH public key line. A test-only copy of the same three-liner that
/// lives in `crates/api/src/credentials.rs` (private there) — the old facade's `auth` module,
/// which used to re-export it for these tests, is gone.
pub fn ssh_fingerprint(line: &str) -> Result<String, ()> {
    let key = russh::keys::PublicKey::from_openssh(line.trim()).map_err(|_| ())?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

/// Serve `app` on a loopback port and return the port.
pub async fn serve(app: Arc<kloudlite_app::App>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(l, kloudlite_server::router::router(app)).await.unwrap();
    });
    port
}

pub async fn serve_public() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, kloudlite_server::router::router(app)).await.unwrap();
    });
    (base, e)
}


/// The PEER router, where the browse API lives. Requests to it must carry the shared secret,
/// which `peer_get` adds.
pub async fn serve_peer() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, kloudlite_server::router::peer_router(app)).await.unwrap();
    });
    (base, e)
}

/// Both routers, one store: the image-delete tests need to push a manifest and a blob over the
/// PUBLIC registry protocol, then reach the PEER-only browse writes (`imagetagdelete`,
/// `imagedelete`) that this task adds — and confirm the deletes stuck by reading back over the
/// public router again. Two listeners sharing one `Arc<App>` is the only way to do that in-process.
pub async fn serve_public_and_peer() -> (String, String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let pub_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pub_base = format!("http://{}", pub_l.local_addr().unwrap());
    let peer_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_base = format!("http://{}", peer_l.local_addr().unwrap());
    let app2 = app.clone();
    tokio::spawn(async move {
        axum::serve(pub_l, kloudlite_server::router::router(app2)).await.unwrap();
    });
    tokio::spawn(async move {
        axum::serve(peer_l, kloudlite_server::router::peer_router(app)).await.unwrap();
    });
    (pub_base, peer_base, e)
}

pub async fn peer_get(base: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
        .send()
        .await
        .unwrap()
}

/// Like `peer_get`, plus the owner header the api tier attaches once it has verified who is
/// asking (see `browse_caller` in `src/api.rs`). The owner-scoped browse routes (`images`) check
/// this identity themselves, so a direct peer test must present it exactly as the api tier would.
pub async fn peer_get_as(base: &str, owner: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
        .header(kloudlite_core::peer::OWNER_HEADER, owner)
        .send()
        .await
        .unwrap()
}

/// Like `peer_get_as`, for the browse WRITES (`imagetagdelete`, `imagedelete`): same identity
/// headers, POST instead of GET, and a body the caller (a tag name, or nothing) supplies.
pub async fn peer_post_as(base: &str, owner: &str, path: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header(kloudlite_core::peer::PEER_HEADER, "test-peer-secret")
        .header(kloudlite_core::peer::OWNER_HEADER, owner)
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

/// A repo with two commits (the second edits `src/main.rs`), pushed in over the real receive-pack
/// path so the objects land as packs exactly as they would in production. Returns it opened.
pub async fn push_fixture(e: &TestEnv, owner: &str, name: &str) -> kloudlite_storage::store::Repo {
    push_built(e, owner, name, |c| {
        std::fs::create_dir(c.join("src")).unwrap();
        std::fs::write(c.join("README.md"), "hello\n").unwrap();
        std::fs::write(c.join("src/main.rs"), "fn main() {\n    println!(\"one\");\n}\n").unwrap();
        git(c, &["add", "."]);
        git(c, &["commit", "-qm", "one"]);
        std::fs::write(c.join("src/main.rs"), "fn main() {\n    println!(\"two\");\n}\n").unwrap();
        git(c, &["commit", "-qam", "two"]);
    })
    .await
}

/// `build` makes the commits in a fresh work tree; everything on HEAD is then pushed to
/// `refs/heads/master` of a new repo, and the opened repo returned.
pub async fn push_built(
    e: &TestEnv,
    owner: &str,
    name: &str,
    build: impl FnOnce(&std::path::Path),
) -> kloudlite_storage::store::Repo {
    let s = e.store.clone();
    s.create_repo(owner, name).await.unwrap();
    let token = s.create_token(owner).await.unwrap();
    let port = serve(app(s.clone()).await).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/{owner}/{name}.git");

    let w = tempfile::tempdir().unwrap();
    let c = w.path().join("work");
    std::fs::create_dir(&c).unwrap();
    git(&c, &["init", "-q"]);
    // A machine with no global user.email cannot commit at all; the fixture carries its own.
    git(&c, &["config", "user.email", "t@t"]);
    git(&c, &["config", "user.name", "t"]);
    build(&c);
    git(&c, &["push", "-q", &url, "HEAD:refs/heads/master"]);

    s.open_repo(owner, name).await.unwrap().unwrap()
}

/// Like `push_built`, but pushes EVERY local branch and returns the peer listener's base URL —
/// the shape the merge worker needs: more than one branch to combine, and a fleet to fetch from
/// and push back to.
pub async fn push_branches(
    e: &TestEnv,
    owner: &str,
    name: &str,
    build: impl FnOnce(&std::path::Path),
) -> String {
    let s = e.store.clone();
    s.create_repo(owner, name).await.unwrap();
    let token = s.create_token(owner).await.unwrap();
    let app = app(s.clone()).await;
    let port = serve(app.clone()).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/{owner}/{name}.git");

    let w = tempfile::tempdir().unwrap();
    let c = w.path().join("work");
    std::fs::create_dir(&c).unwrap();
    git(&c, &["init", "-q"]);
    git(&c, &["config", "user.email", "t@t"]);
    git(&c, &["config", "user.name", "t"]);
    build(&c);
    git(&c, &["push", "-q", &url, "--all"]);

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, kloudlite_server::router::peer_router(app)).await.unwrap();
    });
    base
}

/// Puts each of `contents` into `owner`'s blob store directly, so a test manifest can name layers
/// without pushing them over HTTP — `put_manifest` refuses a manifest naming a blob it cannot find.
pub async fn seed_blobs(e: &TestEnv, owner: &str, contents: &[&[u8]]) {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};
    for c in contents {
        let d = kloudlite_registry::Digest::of(c);
        e.store
            .os
            .put(&kloudlite_registry::store::blob_path(owner, &d), PutPayload::from(c.to_vec()))
            .await
            .unwrap();
    }
}

// ── a real directory ────────────────────────────────────────────────────────
//
// The directory is a concrete Mongo struct, so the `/v1` handlers behind it can only be reached
// with a Mongo behind them. A trait and an in-memory double would be ~40 methods of bson-shaped
// signatures for one test dependency, so the tests run against a real server instead: CI provides
// one (`image.yml`'s `test` job), and a laptop without `KLOUDLITE_TEST_MONGO_URI` skips the
// handler half and keeps only the gate half, which needs no database at all.

/// A `Directory` on a database of this test's own, dropped when the fixture is.
pub struct TestDirectory {
    pub dir: Arc<kloudlite_pulls::directory::Directory>,
    client: mongodb::Client,
    name: String,
}

/// `Some` when `KLOUDLITE_TEST_MONGO_URI` names a Mongo, `None` — with a printed reason — when
/// it does not. `what` names the test, so a skipped run says which coverage was not taken.
pub async fn mongo(what: &str) -> Option<TestDirectory> {
    let uri = match std::env::var("KLOUDLITE_TEST_MONGO_URI") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skipping the handler half of {what}: KLOUDLITE_TEST_MONGO_URI is unset");
            return None;
        }
    };
    // pid and clock keep two concurrent `cargo test` runs (or two CI jobs on one server) apart;
    // the counter keeps two fixtures within one run apart, which the clock alone would not.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let name = format!(
        "rgtest-{}-{millis}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let dir = kloudlite_pulls::directory::Directory::connect(&uri, &name).await.unwrap();
    let client = mongodb::Client::with_uri_str(&uri).await.unwrap();
    Some(TestDirectory { dir: Arc::new(dir), client, name })
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let (client, name) = (self.client.clone(), self.name.clone());
        // `Drop` cannot await. Every test that takes this fixture is `multi_thread`, so the
        // runtime can spare a thread for the one round trip that throws the database away.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let _ = client.database(&name).drop().await;
            })
        });
    }
}
