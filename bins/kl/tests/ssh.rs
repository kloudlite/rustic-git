//! `kl ws ssh <name>` used to make three api calls before ssh even started (list to resolve the
//! name, mint, then the ProxyCommand minted again). Now it is one: the api resolves the name, and
//! the session reaches the ProxyCommand through ssh's environment. A fake `ssh` on PATH shows
//! what the real one would have been handed.

mod stub;

use std::process::Command;

#[cfg(unix)]
#[test]
fn ssh_makes_one_api_call_and_hands_the_session_to_the_proxy_by_env() {
    use std::os::unix::fs::PermissionsExt;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let s = stub::Stub::default();
    let api = rt.block_on(stub::spawn(s.clone()));
    let cfg = tempfile::tempdir().unwrap();
    stub::write_config(cfg.path(), &api);

    let bin = tempfile::tempdir().unwrap();
    let fake = bin.path().join("ssh");
    std::fs::write(&fake, "#!/bin/sh\necho \"ARGS $*\"\necho \"SESSION ${KL_SSH_SESSION:-none}\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());

    let out = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["ws", "ssh", "gh", "--", "-A"])
        .env("KL_CONFIG_DIR", cfg.path())
        .env("PATH", path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.api_calls.load(std::sync::atomic::Ordering::SeqCst), 1, "{stdout}");
    assert!(stdout.contains("kl@ws-1 -A"), "the name resolved to the id: {stdout}");
    assert!(stdout.contains("HostKeyAlias=ws-1"), "{stdout}");
    let session = stdout.lines().find_map(|l| l.strip_prefix("SESSION ")).unwrap();
    let v: serde_json::Value = serde_json::from_str(session).unwrap();
    assert_eq!(v["id"], "ws-1");
    assert_eq!(v["token"], "sst_test");
    // And the host key was pinned by the parent, where ssh reads it.
    let known = std::fs::read_to_string(cfg.path().join("known_hosts")).unwrap_or_default();
    assert!(known.contains("ws-1"), "{known}");
}
