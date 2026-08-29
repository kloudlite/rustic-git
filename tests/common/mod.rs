#![allow(dead_code)]
use rustic_git_storage::store::Store;
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
/// `RUSTIC_GIT_S3_URL=file://` gives, and object_store has no `MultipartStore` for it. The harness
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
        axum::serve(l, rustic_git_server::router::router(app, no_jobs_state())).await.unwrap();
    });
    (base, e)
}

/// Like `env`, but with an in-process cache instead of a disabled one: the only way a test can
/// observe that a write path invalidated anything without a live Redis.
pub async fn env_cached() -> TestEnv {
    let e = env().await;
    let mut store = Arc::try_unwrap(e.store).ok().unwrap();
    store.cache = Arc::new(rustic_git_storage::cache::Cache::memory());
    TestEnv { store: Arc::new(store), _tmp: e._tmp }
}

/// An App for tests that are not about routing: this node is `rustic-git-0`, so it is its own
/// leader and every claim is decided locally against its own ownership database.
pub async fn app(store: Arc<Store>) -> Arc<rustic_git_app::App> {
    let ownership = rustic_git_storage::ownership::OwnershipStore::open(store.os.clone());
    ownership.promote().await.unwrap();
    Arc::new(rustic_git_app::App::new(
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
    have_tool("git", "--version")
}

pub fn have_ssh() -> bool {
    have_tool("ssh", "-V")
}

/// Every git/ssh-dependent test skips itself when the tool is absent — fine on a laptop, and a
/// silent green on a CI runner missing the tool. CI sets `RUSTIC_GIT_REQUIRE_GIT=1`, which turns
/// the skip into a failure.
fn have_tool(bin: &str, probe: &str) -> bool {
    let ok = std::process::Command::new(bin).arg(probe).output().map(|o| o.status.success()).unwrap_or(false);
    assert!(
        ok || std::env::var_os("RUSTIC_GIT_REQUIRE_GIT").is_none(),
        "{bin} is not installed and RUSTIC_GIT_REQUIRE_GIT is set: this test must not skip here"
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

/// No Cosmos in tests: `authorized` then falls back to the break-glass token list, which is what
/// the record-route tests present.
pub fn no_jobs_state() -> Arc<rustic_git_server::vol_agent::JobsState> {
    Arc::new(rustic_git_server::vol_agent::JobsState::new(None))
}


/// Serve `app` on a loopback port and return the port.
pub async fn serve(app: Arc<rustic_git_app::App>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(l, rustic_git_server::router::router(app, no_jobs_state())).await.unwrap();
    });
    port
}

/// A `JobsState` backed by a real `MemStore`, seeded with `regions` as `(id, agent_token)`
/// pairs — what the record routes need to scope a token to a volume's region.
pub async fn jobs_state_with_regions(regions: &[(&str, &str)]) -> Arc<rustic_git_server::vol_agent::JobsState> {
    use rustic_git_workspaces::store::MetaStore;
    let meta = std::sync::Arc::new(rustic_git_workspaces::store::MemStore::new());
    for (id, token) in regions {
        meta.put_region(&rustic_git_workspaces::model::Region {
            id: (*id).to_string(),
            name: (*id).to_string(),
            storage_account: "acct".into(),
            blob_container: "cont".into(),
            status: "active".into(),
            agent_token: (*token).to_string(),
        })
        .await
        .unwrap();
    }
    Arc::new(rustic_git_server::vol_agent::JobsState::new(Some(meta as std::sync::Arc<dyn MetaStore>)))
}

/// Serve the PUBLIC router with `jobs_state_with_regions` on an ephemeral port. Returns its base
/// URL and the env behind it.
pub async fn serve_public_with_regions(regions: &[(&str, &str)]) -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let jobs = jobs_state_with_regions(regions).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, rustic_git_server::router::router(app, jobs)).await.unwrap();
    });
    (base, e)
}

pub async fn serve_public() -> (String, TestEnv) {
    let e = env().await;
    let app = app(e.store.clone()).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, rustic_git_server::router::router(app, no_jobs_state())).await.unwrap();
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
        axum::serve(l, rustic_git_server::router::peer_router(app, no_jobs_state())).await.unwrap();
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
        axum::serve(pub_l, rustic_git_server::router::router(app2, no_jobs_state())).await.unwrap();
    });
    tokio::spawn(async move {
        axum::serve(peer_l, rustic_git_server::router::peer_router(app, no_jobs_state())).await.unwrap();
    });
    (pub_base, peer_base, e)
}

pub async fn peer_get(base: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
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
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, owner)
        .send()
        .await
        .unwrap()
}

/// Like `peer_get_as`, for the browse WRITES (`imagetagdelete`, `imagedelete`): same identity
/// headers, POST instead of GET, and a body the caller (a tag name, or nothing) supplies.
pub async fn peer_post_as(base: &str, owner: &str, path: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header(rustic_git_core::peer::PEER_HEADER, "test-peer-secret")
        .header(rustic_git_core::peer::OWNER_HEADER, owner)
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

/// A repo with two commits (the second edits `src/main.rs`), pushed in over the real receive-pack
/// path so the objects land as packs exactly as they would in production. Returns it opened.
pub async fn push_fixture(e: &TestEnv, owner: &str, name: &str) -> rustic_git_storage::store::Repo {
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
) -> rustic_git_storage::store::Repo {
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
        axum::serve(l, rustic_git_server::router::peer_router(app, no_jobs_state())).await.unwrap();
    });
    base
}

/// Puts each of `contents` into `owner`'s blob store directly, so a test manifest can name layers
/// without pushing them over HTTP — `put_manifest` refuses a manifest naming a blob it cannot find.
pub async fn seed_blobs(e: &TestEnv, owner: &str, contents: &[&[u8]]) {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};
    for c in contents {
        let d = rustic_git_registry::Digest::of(c);
        e.store
            .os
            .put(&rustic_git_registry::store::blob_path(owner, &d), PutPayload::from(c.to_vec()))
            .await
            .unwrap();
    }
}
