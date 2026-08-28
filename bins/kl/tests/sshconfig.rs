//! `kl ws ssh-config` against a stub api: the rendering is a contract (people read and edit these
//! files), and the `Include` line must survive being written twice.

mod stub;

use std::process::Command;

#[test]
fn renders_a_block_per_workspace_and_includes_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let api = rt.block_on(stub::spawn(stub::Stub));

    let home = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/config"), "Host old\n  User me\n").unwrap();
    stub::write_config(cfg.path(), &api);

    let run = || {
        let out = Command::new(env!("CARGO_BIN_EXE_kl"))
            .args(["ws", "ssh-config"])
            .env("HOME", home.path())
            .env("KL_CONFIG_DIR", cfg.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    run();

    let known = cfg.path().join("known_hosts").display().to_string();
    let want = format!(
        "# Managed by kl. Edits are overwritten by `kl ws ssh-config`.\n\
         \n\
         Host gh\n  \
           HostName ws-1\n  \
           User root\n  \
           ProxyCommand kl ws proxy ws-1\n  \
           UserKnownHostsFile {known}\n  \
           HostKeyAlias ws-1\n\
         \n\
         Host api\n  \
           HostName ws-2\n  \
           User root\n  \
           ProxyCommand kl ws proxy ws-2\n  \
           UserKnownHostsFile {known}\n  \
           HostKeyAlias ws-2\n"
    );
    let got = std::fs::read_to_string(home.path().join(".ssh/kloudlite_config")).unwrap();
    assert_eq!(got, want);

    run();
    let ssh_config = std::fs::read_to_string(home.path().join(".ssh/config")).unwrap();
    let include = format!("Include {}/.ssh/kloudlite_config", home.path().display());
    assert_eq!(ssh_config.matches(&include).count(), 1, "include added once: {ssh_config}");
    assert!(ssh_config.starts_with(&include), "include must be first: {ssh_config}");
    assert!(ssh_config.contains("Host old"), "the user's own config is kept: {ssh_config}");
}
