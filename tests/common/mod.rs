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
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
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
