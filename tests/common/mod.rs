#![allow(dead_code)]
use rustic_git::store::Store;
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;

pub struct TestEnv {
    pub store: Arc<Store>,
    pub _tmp: tempfile::TempDir,
}

pub async fn env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(
        Arc::new(InMemory::new()),
        tmp.path().join("cache"),
        false,
    )
    .await
    .unwrap();
    TestEnv {
        store: Arc::new(store),
        _tmp: tmp,
    }
}

/// Like `env`, but with an in-process cache instead of a disabled one: the only way a test can
/// observe that a write path invalidated anything without a live Redis.
pub async fn env_cached() -> TestEnv {
    let e = env().await;
    let mut store = Arc::try_unwrap(e.store).ok().unwrap();
    store.cache = Arc::new(rustic_git::cache::Cache::memory());
    TestEnv { store: Arc::new(store), _tmp: e._tmp }
}

/// An App for tests that are not about routing: this node is `rustic-git-0`, so it is its own
/// leader and every claim is decided locally against its own ownership database.
pub async fn app(store: Arc<Store>) -> Arc<rustic_git::App> {
    let ownership = rustic_git::ownership::OwnershipStore::open(store.os.clone(), true)
        .await
        .unwrap();
    Arc::new(rustic_git::App::new(
        store,
        Arc::new(ownership),
        "rustic-git-0".into(),
        // Nothing is ever forwarded here: this node owns whatever it claims.
        Arc::new(|_| "127.0.0.1:1".to_string()),
        "test-peer-secret".into(),
        // One node: the leader serves, because there is no one else to hand a repo to.
        1,
    ))
}

pub fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

pub fn have_ssh() -> bool {
    std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Serve `app` on a loopback port and return the port.
pub async fn serve(app: Arc<rustic_git::App>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(l, rustic_git::http::router(app)).await.unwrap();
    });
    port
}

/// Serve the PUBLIC router on an ephemeral port. Returns its base URL and the env behind it.
pub async fn serve_public() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, rustic_git::http::router(app)).await.unwrap();
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
        axum::serve(l, rustic_git::http::peer_router(app)).await.unwrap();
    });
    (base, e)
}

pub async fn peer_get(base: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
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
        .header(rustic_git::proxy::PEER_HEADER, "test-peer-secret")
        .header(rustic_git::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .unwrap()
}

/// A repo with two commits (the second edits `src/main.rs`), pushed in over the real receive-pack
/// path so the objects land as packs exactly as they would in production. Returns it opened.
pub async fn push_fixture(e: &TestEnv, owner: &str, name: &str) -> rustic_git::store::Repo {
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
) -> rustic_git::store::Repo {
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
