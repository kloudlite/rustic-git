//! The package list a workspace declares (`spec.packages` on its CRD), and everything the
//! reconciler needs derived from it. Pure on purpose: this module never touches the disk or Nix,
//! so every rule about what a list may say is testable without either.
//!
//! The list arrives from the API — which writes `spec.packages` — but the CR itself is not a
//! trust boundary the API alone controls: any principal with write access to the object (a
//! restored backup, a migration, `kubectl edit`) can put an arbitrary list there. So the same
//! grammar is checked twice: once by the API before it writes, again by the reconciler before it
//! ever renders a name into a Nix expression.

use sha2::{Digest, Sha256};

pub const MAX_PACKAGES: usize = 100;
pub const MAX_ATTR_LEN: usize = 64;
/// Inside the pod, where the workspace's own profile is mounted.
pub const PROFILE_MOUNT: &str = "/nix/profile";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Debug, PartialEq)]
pub enum PackageError {
    Attr(String),
    TooMany(usize),
    Duplicate(String),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::Attr(a) => write!(f, "{a:?} is not a package attribute name"),
            PackageError::TooMany(n) => write!(f, "{n} packages; the limit is {MAX_PACKAGES}"),
            PackageError::Duplicate(a) => write!(f, "{a:?} is listed twice"),
        }
    }
}

pub fn validate_attr(s: &str) -> Result<(), PackageError> {
    let mut chars = s.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'));
    if ok_first && ok_rest && s.len() <= MAX_ATTR_LEN {
        Ok(())
    } else {
        Err(PackageError::Attr(s.to_string()))
    }
}

/// Validates a whole list: size, grammar of every entry, and no duplicates.
pub fn validate_list(list: &[String]) -> Result<(), PackageError> {
    if list.len() > MAX_PACKAGES {
        return Err(PackageError::TooMany(list.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for p in list {
        validate_attr(p)?;
        if !seen.insert(p.as_str()) {
            return Err(PackageError::Duplicate(p.clone()));
        }
    }
    Ok(())
}

/// What the profile on disk IS: the pin and the sorted list. Sorted so a reordered file is not a
/// rebuild; pinned so a rolled nixpkgs is.
pub fn hash(pin: &str, packages: &[String]) -> String {
    let mut sorted: Vec<&str> = packages.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    for p in sorted {
        h.update(b"\n");
        h.update(p.as_bytes());
    }
    format!("sha256:{:x}", h.finalize())
}

/// The whole expression `nix build --expr` evaluates. Names arrive validated (`validate_attr`)
/// and are emitted as `pkgs.<name>` inside a list literal — there is no string context in the
/// expression a name could escape into.
pub fn expression(pin: &str, id: &str, packages: &[String]) -> String {
    let paths: Vec<String> = packages.iter().map(|p| format!("pkgs.{p}")).collect();
    format!(
        "let pkgs = import (builtins.getFlake \"{pin}\") {{ }}; in pkgs.buildEnv {{ name = \"ws-{id}-env\"; paths = [ {} ]; }}",
        paths.join(" ")
    )
}

/// The image's own PATH is unknown to us at apply time — the kubelet only merges env on top of
/// the image's — so the container gets an explicit one: profile first, then a default that every
/// Debian/Alpine image already has.
pub fn path_env(image_path: Option<&str>) -> String {
    format!("{PROFILE_MOUNT}/bin:{}", image_path.unwrap_or(DEFAULT_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attribute_grammar_refuses_anything_that_could_be_code() {
        for bad in ["$(id)", "a b", "a\"b", "a;b", "(x)", "-lead", "", &"x".repeat(65)] {
            assert!(validate_attr(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in ["hello", "nodejs_20", "python3Packages.requests", "gcc-wrapper", "libc++"] {
            assert!(validate_attr(ok).is_ok(), "{ok:?} must pass");
        }
    }

    #[test]
    fn a_list_is_validated_as_a_whole() {
        assert!(validate_list(&["hello".into(), "jq".into()]).is_ok());
        assert!(matches!(validate_list(&["hello".into(), "hello".into()]), Err(PackageError::Duplicate(_))));
        let many: Vec<String> = (0..101).map(|i| format!("p{i}")).collect();
        assert!(matches!(validate_list(&many), Err(PackageError::TooMany(101))));
        assert!(matches!(validate_list(&["$(id)".into()]), Err(PackageError::Attr(_))));
    }

    #[test]
    fn the_hash_is_order_independent_and_pin_sensitive() {
        let a = hash("github:NixOS/nixpkgs/aaaa", &["go".into(), "jq".into()]);
        let b = hash("github:NixOS/nixpkgs/aaaa", &["jq".into(), "go".into()]);
        let c = hash("github:NixOS/nixpkgs/bbbb", &["go".into(), "jq".into()]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn the_expression_is_a_list_literal_never_interpolated_text() {
        let e = expression("github:NixOS/nixpkgs/aaaa", "ws-1", &["go".into(), "python3Packages.requests".into()]);
        assert_eq!(
            e,
            "let pkgs = import (builtins.getFlake \"github:NixOS/nixpkgs/aaaa\") { }; in pkgs.buildEnv { name = \"ws-ws-1-env\"; paths = [ pkgs.go pkgs.python3Packages.requests ]; }"
        );
        let empty = expression("github:NixOS/nixpkgs/aaaa", "ws-1", &[]);
        assert!(empty.contains("paths = [  ];"));
    }

    #[test]
    fn path_env_prepends_the_profile_and_falls_back_to_a_sane_default() {
        assert_eq!(path_env(Some("/opt/bin:/usr/bin")), "/nix/profile/bin:/opt/bin:/usr/bin");
        assert_eq!(path_env(None), "/nix/profile/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
}
