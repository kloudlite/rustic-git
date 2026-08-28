//! The workspace host keypair, made by `ssh-keygen` rather than in Rust.
//!
//! Behind a trait for the same reason `nix::Nix` is: the reconciler is tested with a fake, and the
//! real one shells out to a binary that only exists in the agent image.

/// `(private key in OpenSSH format, public key line)`.
pub trait HostKeys: Send + Sync {
    fn generate(&self) -> Result<(String, String), String>;
}

pub struct SshKeygen;

impl HostKeys for SshKeygen {
    fn generate(&self) -> Result<(String, String), String> {
        // A tempdir, not a fixed path: `ssh-keygen` refuses to overwrite and the agent's root
        // filesystem is read-only. The directory (and the private key with it) is removed on drop,
        // so the only lasting copy is the Secret the caller writes.
        let dir = tempfile::tempdir().map_err(|e| format!("host key tempdir: {e}"))?;
        let key = dir.path().join("key");
        let out = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "ws", "-f"])
            .arg(&key)
            .output()
            .map_err(|e| format!("ssh-keygen: {e}"))?;
        if !out.status.success() {
            return Err(format!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr)));
        }
        let private = std::fs::read_to_string(&key).map_err(|e| format!("read host key: {e}"))?;
        let public = std::fs::read_to_string(key.with_extension("pub")).map_err(|e| format!("read host key: {e}"))?;
        Ok((private, public.trim().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one check that the argv is right: a key `sshd` would accept, and a public line the CLI
    /// can put in `known_hosts` verbatim.
    #[test]
    fn ssh_keygen_makes_an_ed25519_pair() {
        let Ok((private, public)) = SshKeygen.generate() else {
            return; // no ssh-keygen on this machine; the agent image installs one
        };
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"), "{private}");
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        assert!(!public.contains('\n'), "one line, as known_hosts wants: {public}");
    }
}
