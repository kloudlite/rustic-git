pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}

/// Fleet mode may not fall back to a per-process JWT secret.
///
/// `App::new` invents a random secret when `RUSTIC_GIT_JWT_SECRET` is unset. On one node that is
/// harmless — tokens die with the process. Across a fleet each node invents a DIFFERENT one, so a
/// token minted by `srv-0` is a forgery to `srv-1`: registry pulls fail on whichever node the load
/// balancer picks next, intermittently, which is the worst possible way to learn about it. A fleet
/// is exactly what `RUSTIC_GIT_PEER_SVC` marks, so that is the condition. Same shape as the
/// `RUSTIC_GIT_S3_URL=file://` fleet check in main.rs: refuse to start, and name the variable.
///
/// Takes its inputs rather than reading the environment so the rule is testable and so both
/// binaries apply the same one.
pub fn require_jwt_secret(peer_svc: &str, jwt_secret: &str) -> Result<()> {
    if !peer_svc.is_empty() && jwt_secret.is_empty() {
        return Err(err(
            "RUSTIC_GIT_JWT_SECRET is required with RUSTIC_GIT_PEER_SVC (without it each node \
             mints tokens the others reject)",
        ));
    }
    Ok(())
}

/// Reads the two variables `require_jwt_secret` judges, so a caller cannot get the pair wrong.
pub fn require_jwt_secret_from_env() -> Result<()> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    require_jwt_secret(var("RUSTIC_GIT_PEER_SVC").trim(), var("RUSTIC_GIT_JWT_SECRET").trim())
}

/// Lowercase hex, the encoding every digest, fingerprint and token id in this crate uses on the
/// wire. One definition so a future change (or a faster one) happens in one place. `pub` rather
/// than `pub(crate)` only because `main.rs` is a separate crate and mints the peer secret with it.
pub fn hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_mode_refuses_a_missing_jwt_secret() {
        assert!(require_jwt_secret("rustic-git-peer", "").is_err());
        // Solo mode has nobody to disagree with, so the per-process fallback stays.
        assert!(require_jwt_secret("", "").is_ok());
        assert!(require_jwt_secret("rustic-git-peer", "s3cret").is_ok());
    }

    #[test]
    fn hex_is_lowercase_and_two_chars_per_byte() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[]), "");
    }
}
