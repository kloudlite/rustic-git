//! The workspace host keypair: a fresh ed25519 key in OpenSSH format, generated in-process.
//!
//! Byte-format-compatible with `ssh-keygen -t ed25519 -N ""` — an unencrypted OpenSSH private key
//! and a one-line `ssh-ed25519 ...` public half — because `sshd` in the workspace pod reads it.

use ssh_key::{rand_core::UnwrapErr, Algorithm, LineEnding, PrivateKey};

/// `(private key in OpenSSH format, public key line)`.
pub fn generate() -> Result<(String, String), String> {
    let mut key = PrivateKey::random(&mut UnwrapErr(ssh_key::getrandom::SysRng), Algorithm::Ed25519)
        .map_err(|e| format!("host key: {e}"))?;
    key.set_comment("ws");
    // LF, not CRLF: the public half is pinned verbatim into a `known_hosts` line by the CLI.
    let private = key.to_openssh(LineEnding::LF).map_err(|e| format!("host key: {e}"))?;
    let public = key.public_key().to_openssh().map_err(|e| format!("host key: {e}"))?;
    Ok((private.to_string(), public))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key `sshd` would accept, and a public line the CLI can put in `known_hosts` verbatim.
    #[test]
    fn generate_makes_an_ed25519_pair() {
        let (private, public) = generate().expect("keygen is pure");
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"), "{private}");
        assert!(private.ends_with("-----END OPENSSH PRIVATE KEY-----\n"), "{private}");
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        assert!(!public.contains('\n'), "one line, as known_hosts wants: {public}");
        // Unencrypted, and it round-trips through the same parser sshd uses.
        let back = PrivateKey::from_openssh(&private).expect("openssh format");
        assert!(!back.is_encrypted());
        assert_eq!(back.public_key().to_openssh().unwrap(), public);
        // Two calls are two keys — a fixed key would give every workspace the same identity.
        assert_ne!(generate().unwrap().1, public);
    }
}
