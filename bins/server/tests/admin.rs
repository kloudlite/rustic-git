//! Spawns the real binary, so it lives in the binary's own package: `CARGO_BIN_EXE_` is only
//! set for same-package bins, which is what guarantees cargo builds it before the test runs
//! (the root test host's path-guessing broke on a cold target dir in CI).

/// Catches: a missing `admin purge-cache` arm, which falls through to the usage error.
#[test]
fn purge_cache_is_a_command() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_kloudlite-git"))
        .args(["admin", "purge-cache", "alice/web"])
        .env("KLOUDLITE_GIT_S3_URL", "mem://")
        .env("KLOUDLITE_GIT_CACHE_DIR", tempfile::tempdir().unwrap().keep())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "purge-cache failed: {err}");
    assert!(!err.contains("usage:"), "purge-cache fell through to usage: {err}");
}
