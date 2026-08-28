//! `~/.ssh/kloudlite_config`: a generated file, plus one `Include` line in the config the user
//! owns. Regenerating must be safe, so the block file is rewritten whole and the Include is
//! added only when it is not already there.

use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn render(workspaces: &[crate::api::Workspace], known_hosts: &std::path::Path) -> String {
    let mut s = String::from("# Managed by kl. Edits are overwritten by `kl ws ssh-config`.\n");
    for w in workspaces {
        s.push_str(&format!(
            "\nHost {name}\n  HostName {id}\n  User root\n  ProxyCommand kl ws proxy {id}\n  \
             UserKnownHostsFile {kh}\n  HostKeyAlias {id}\n",
            name = w.name,
            id = w.id,
            kh = known_hosts.display(),
        ));
    }
    s
}

pub fn write(workspaces: &[crate::api::Workspace]) -> Result<PathBuf, String> {
    let ssh = home().join(".ssh");
    std::fs::create_dir_all(&ssh).map_err(|e| e.to_string())?;
    let block = ssh.join("kloudlite_config");
    std::fs::write(&block, render(workspaces, &crate::config::known_hosts()))
        .map_err(|e| e.to_string())?;

    // ssh takes the FIRST value it sees for a keyword, so an Include appended after the user's own
    // `Host *` block would silently lose to it.
    let cfg = ssh.join("config");
    let include = format!("Include {}\n", block.display());
    let existing = std::fs::read_to_string(&cfg).unwrap_or_default();
    if !existing.contains(include.trim_end()) {
        std::fs::write(&cfg, format!("{include}{existing}")).map_err(|e| e.to_string())?;
    }
    Ok(block)
}
