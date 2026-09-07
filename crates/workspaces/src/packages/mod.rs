//! The package list a workspace declares (`spec.packages` on its CRD), and everything the
//! reconciler needs derived from it. Pure on purpose: this module never touches the disk or Nix,
//! so every rule about what a list may say is testable without either.
//!
//! The list arrives from the API — which writes `spec.packages` — but the CR itself is not a
//! trust boundary the API alone controls: any principal with write access to the object (a
//! restored backup, a migration, `kubectl edit`) can put an arbitrary list there. So the same
//! grammar is checked twice: once by the API before it writes, again by the reconciler before it
//! ever renders a name into a Nix expression.

pub mod resolve;

use sha2::{Digest, Sha256};

pub const MAX_PACKAGES: usize = 100;
pub const MAX_ATTR_LEN: usize = 64;
/// Inside the pod, where the workspace's own profile DIRECTORY is mounted.
pub const PROFILE_MOUNT: &str = "/nix/profile";
/// The link inside it that every environment variable points at. The mount cannot be the link
/// itself: the kubelet resolves a subPath once at container start, so a swapped link under a
/// mounted subPath would never reach a running pod — the swap has to happen one level below.
pub const PROFILE_LINK: &str = "/nix/profile/current";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Debug, PartialEq)]
pub enum PackageError {
    Attr(String),
    TooMany(usize),
    Duplicate(String),
    Version(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionReq {
    Latest,
    Prefix(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub attr: String,
    pub version: Option<VersionReq>,
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::Attr(a) => write!(f, "{a:?} is not a package attribute name"),
            PackageError::TooMany(n) => write!(f, "{n} packages; the limit is {MAX_PACKAGES}"),
            PackageError::Duplicate(a) => write!(f, "{a:?} is listed twice"),
            PackageError::Version(e) => write!(f, "{e:?} is not a version: use latest, N, N.N or N.N.N"),
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

/// `attr` or `attr@version`. The version grammar is devbox's: `latest`, or one to three dotted
/// numbers, digits only — a prefix, resolved to the newest release under it. Anything with an
/// operator or a pre-release tag is refused: there is exactly one way to write a pin, so a list is
/// never ambiguous about what it asked for.
pub fn parse_entry(s: &str) -> Result<Entry, PackageError> {
    if s.len() > MAX_ATTR_LEN {
        return Err(PackageError::Attr(s.to_string()));
    }
    let Some((attr, version)) = s.split_once('@') else {
        validate_attr(s)?;
        return Ok(Entry { attr: s.to_string(), version: None });
    };
    validate_attr(attr)?;
    let req = if version == "latest" {
        VersionReq::Latest
    } else {
        let parts: Vec<&str> = version.split('.').collect();
        let ok = (1..=3).contains(&parts.len())
            && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !ok {
            return Err(PackageError::Version(s.to_string()));
        }
        VersionReq::Prefix(version.to_string())
    };
    Ok(Entry { attr: attr.to_string(), version: Some(req) })
}

/// Validates a whole list: size, grammar of every entry, and no duplicates.
///
/// Duplicates are keyed on the ATTRIBUTE, not the entry string: `nodejs` and `nodejs@20` are two
/// requests for one profile entry, and Nix has no way to install both.
pub fn validate_list(list: &[String]) -> Result<(), PackageError> {
    if list.len() > MAX_PACKAGES {
        return Err(PackageError::TooMany(list.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for p in list {
        let entry = parse_entry(p)?;
        if !seen.insert(entry.attr.clone()) {
            return Err(PackageError::Duplicate(entry.attr));
        }
    }
    Ok(())
}

/// The entries that name no version: they come straight from the pinned nixpkgs, so they need no
/// resolution and no lock row.
pub fn bare(list: &[String]) -> Vec<String> {
    list.iter().filter(|p| !p.contains('@')).cloned().collect()
}

/// The entries that DO name a version, each with its parsed form. Unparsable entries are dropped
/// rather than reported — every caller here has already run `validate_list`.
pub fn pinned(list: &[String]) -> Vec<(String, Entry)> {
    list.iter()
        .filter(|p| p.contains('@'))
        .filter_map(|p| parse_entry(p).ok().map(|e| (p.clone(), e)))
        .collect()
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
///
/// The name carries NO workspace id. It used to (`ws-{id}-env`), which put the id in the
/// derivation and therefore in the store path, so two workspaces with identical inputs built two
/// identical-but-separate profiles and a clone could never reuse its source's. Keyed only on what
/// it contains, one store path serves every workspace that asks for the same set.
pub fn expression(pin: &str, packages: &[String]) -> String {
    let paths: Vec<String> = packages.iter().map(|p| format!("pkgs.{p}")).collect();
    format!(
        "let pkgs = import (builtins.getFlake \"{pin}\") {{ }}; in pkgs.buildEnv {{ name = \"kloudlite-workspace-env\"; paths = [ {} ]; }}",
        paths.join(" ")
    )
}

/// The image's own PATH is unknown to us at apply time — the kubelet only merges env on top of
/// the image's — so the container gets an explicit one: profile first, then a default that every
/// Debian/Alpine image already has.
pub fn path_env(image_path: Option<&str>) -> String {
    format!("{PROFILE_LINK}/bin:{}", image_path.unwrap_or(DEFAULT_PATH))
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
    fn an_entry_is_an_attr_or_an_attr_at_a_version() {
        assert_eq!(parse_entry("jq").unwrap(), Entry { attr: "jq".into(), version: None });
        assert_eq!(parse_entry("nodejs@latest").unwrap().version, Some(VersionReq::Latest));
        assert_eq!(parse_entry("nodejs@20").unwrap().version, Some(VersionReq::Prefix("20".into())));
        assert_eq!(parse_entry("python3@3.11.4").unwrap().version, Some(VersionReq::Prefix("3.11.4".into())));
        for bad in ["nodejs@", "nodejs@^20", "nodejs@20.x", "nodejs@20-rc1", "nodejs@>=20", "nodejs@1.2.3.4", "@20", "a@b@c"] {
            assert!(matches!(parse_entry(bad), Err(PackageError::Version(_)) | Err(PackageError::Attr(_))), "{bad:?} must be refused");
        }
    }

    #[test]
    fn duplicates_are_keyed_on_the_attr_not_the_entry() {
        assert!(matches!(validate_list(&["nodejs".into(), "nodejs@20".into()]), Err(PackageError::Duplicate(a)) if a == "nodejs"));
        assert!(validate_list(&["nodejs@20".into(), "jq".into()]).is_ok());
    }

    #[test]
    fn bare_and_pinned_split_a_list() {
        let l = ["jq".to_string(), "nodejs@20".to_string(), "python3@latest".to_string()];
        assert_eq!(bare(&l), vec!["jq"]);
        assert_eq!(pinned(&l).iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), vec!["nodejs@20", "python3@latest"]);
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
        let e = expression("github:NixOS/nixpkgs/aaaa", &["go".into(), "python3Packages.requests".into()]);
        assert_eq!(
            e,
            "let pkgs = import (builtins.getFlake \"github:NixOS/nixpkgs/aaaa\") { }; in pkgs.buildEnv { name = \"kloudlite-workspace-env\"; paths = [ pkgs.go pkgs.python3Packages.requests ]; }"
        );
        let empty = expression("github:NixOS/nixpkgs/aaaa", &[]);
        assert!(empty.contains("paths = [  ];"));
    }

    /// Two workspaces with the same inputs must produce the SAME derivation, or the store cannot
    /// share it and a clone rebuilds what its source already has.
    #[test]
    fn the_expression_does_not_depend_on_which_workspace_asked() {
        let a = expression("github:NixOS/nixpkgs/aaaa", &["go".into()]);
        let b = expression("github:NixOS/nixpkgs/aaaa", &["go".into()]);
        assert_eq!(a, b);
        assert!(!a.contains("ws-"), "the workspace id must not reach the derivation name: {a}");
    }

    #[test]
    fn path_env_prepends_the_profile_and_falls_back_to_a_sane_default() {
        assert_eq!(path_env(Some("/opt/bin:/usr/bin")), "/nix/profile/current/bin:/opt/bin:/usr/bin");
        assert_eq!(path_env(None), "/nix/profile/current/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
}
