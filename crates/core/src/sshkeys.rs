//! OpenSSH public-key helpers. Here rather than in either consumer because the server binary and
//! the api tier both need the same parse, and `storage` — which must stay free of the ssh
//! dependency — takes core with no features.

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it. Used
/// to validate and identify a key before it is stored.
pub fn ssh_fingerprint(line: &str) -> crate::Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

#[cfg(test)]
mod tests {
    /// The one copy: the fingerprint an ssh key is indexed by, and the refusal a non-key gets.
    /// Two hand-mirrored copies of a security-relevant parse is what this module removed.
    #[test]
    fn a_public_key_line_fingerprints_and_anything_else_is_refused() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb9ECWmEzf6FQbrBZ9w7lshQhqowDY5hZYd/Q9K+2sw \
                    alice@example.com";
        let f = super::ssh_fingerprint(line).unwrap();
        assert!(f.starts_with("SHA256:"), "{f}");
        assert_eq!(super::ssh_fingerprint(&format!("  {line}  ")).unwrap(), f, "trimmed");
        assert!(super::ssh_fingerprint("not a key").is_err());
        assert!(super::ssh_fingerprint("").is_err());
    }
}
