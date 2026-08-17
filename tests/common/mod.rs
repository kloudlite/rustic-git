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

/// An App for tests that are not about routing: a one-node fleet where this node is the only peer,
/// so every repo routes Local.
pub fn app(store: Arc<Store>) -> Arc<rustic_git::App> {
    let peers = rustic_git::peers::Membership::fixed(
        vec![rustic_git::peers::Peer {
            name: "solo".into(),
            addr: "127.0.0.1:1".into(),
        }],
        "solo".into(),
    );
    Arc::new(rustic_git::App::new(
        store,
        Arc::new(peers),
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
