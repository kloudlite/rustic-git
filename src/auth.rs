//! The `axum`/`russh`-dependent half of credential handling.
//!
//! Everything else (`Store`'s token/ssh-key storage, the pure `authorize`/`scheme`/`user_names`
//! helpers) moved to `crates/storage/src/auth.rs` along with `Store` itself. This half stays here
//! because `storage` must not depend on `axum` or `russh` (see `crates/storage/Cargo.toml`'s
//! dependency list and its Step 6 verify command) — header parsing needs `axum::http::HeaderMap`,
//! and computing an ssh key's fingerprint needs `russh`. `Store::add_ssh_key` (in `storage`) takes
//! the fingerprint pre-computed, not the raw key line, for the same reason.

pub use rustic_git_storage::auth::*;

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it. Used
/// to validate and identify a key before it is stored — computed here, not as `Store::ssh_fingerprint`
/// as it once was, because it needs `russh` and `Store` now lives in the `russh`-free `storage`
/// crate.
pub fn ssh_fingerprint(line: &str) -> crate::Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

/// The token from a `Bearer` Authorization header.
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    scheme(headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?, "Bearer")
}

/// Both halves of a `Basic` Authorization header.
pub(crate) fn basic_creds(headers: &axum::http::HeaderMap) -> Option<(String, String)> {
    use base64::Engine;
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let d = base64::engine::general_purpose::STANDARD.decode(scheme(v, "Basic")?).ok()?;
    let s = String::from_utf8(d).ok()?;
    s.split_once(':').map(|(u, p)| (u.to_string(), p.to_string()))
}

/// The token inside a `Basic` Authorization header — git's own shape, `x:<token>`, which is what
/// `git clone` over HTTP and `docker login` both send. `None` for no header, another scheme, or
/// anything that does not decode. The one decoder for three callers (git HTTP, the api tier, the
/// registry) — they had drifted into three copies.
pub fn basic_token(headers: &axum::http::HeaderMap) -> Option<String> {
    basic_creds(headers).map(|(_, p)| p)
}

/// Does the `Basic` username name `owner` — the owner its token actually resolved to? A
/// credential whose halves disagree did not verify: a leaked token must not work under any name,
/// and the caller must be refused rather than quietly downgraded to anonymous.
///
/// `true` when no Basic header was sent at all (the credential came as Bearer, which carries no
/// username, and the caller has already decided that is acceptable). `git_placeholder` admits
/// `x`, which every git client sends; the registry passes `false`, because `docker login` always
/// has a real username to send.
pub fn basic_user_names(headers: &axum::http::HeaderMap, owner: &str, git_placeholder: bool) -> bool {
    basic_creds(headers).is_none_or(|(u, _)| user_names(&u, owner, git_placeholder))
}

/// 401 with the Basic challenge git understands. Shared by the git listener and the api tier —
/// two byte-identical copies are one more place for the realm to drift.
pub fn unauthorized() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"rustic-git\"")],
        "auth required",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_token_reads_gits_shape_only() {
        use axum::http::{header, HeaderMap};
        let mut h = HeaderMap::new();
        // "x:secret"
        h.insert(header::AUTHORIZATION, "Basic eDpzZWNyZXQ=".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h).as_deref(), Some("secret"));
        h.insert(header::AUTHORIZATION, "Bearer eDpzZWNyZXQ=".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h), None);
        h.insert(header::AUTHORIZATION, "Basic not-base64!".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h), None);
        assert_eq!(crate::auth::basic_token(&HeaderMap::new()), None);
        // Lowercased by a proxy is the same scheme.
        h.insert(header::AUTHORIZATION, "basic eDpzZWNyZXQ=".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h).as_deref(), Some("secret"));
    }

    /// A token that resolved to `alice` presented under someone else's name did not verify. git's
    /// own `x` placeholder is the exception, and only where a git client speaks.
    #[test]
    fn basic_username_must_name_the_owner() {
        use axum::http::{header, HeaderMap};
        use crate::auth::basic_user_names;
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, v.parse().unwrap());
            h
        };
        let b64 = |s: &str| {
            use base64::Engine;
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(s))
        };
        assert!(basic_user_names(&with(&b64("alice:tok")), "alice", false));
        assert!(!basic_user_names(&with(&b64("mallory:tok")), "alice", true));
        assert!(basic_user_names(&with(&b64("x:tok")), "alice", true));
        assert!(!basic_user_names(&with(&b64("x:tok")), "alice", false));
        // No Basic header: the credential came as Bearer and carries no username.
        assert!(basic_user_names(&HeaderMap::new(), "alice", false));
    }

    /// The common sequence is "ssh fails, add the key, ssh again" — the round trip through
    /// `Store::add_ssh_key` (storage crate) with a fingerprint this crate computed.
    #[tokio::test]
    async fn a_key_added_after_a_failed_login_works_immediately() {
        use rustic_git_storage::store::Store;
        use slatedb::object_store::memory::InMemory;
        use std::sync::Arc;
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMOC8YcsFBuWUwnSZkPymFzXnbPlZth+fBP34XGNN+d test@example.com";
        let fp = crate::auth::ssh_fingerprint(line).unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap(), None);
        store.add_ssh_key("alice", &fp).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap().as_deref(), Some("alice"));
    }
}
