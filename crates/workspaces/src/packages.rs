//! The package list a workspace declares in its repository, and everything the reconciler needs
//! derived from it. Pure on purpose: this module never touches the disk or Nix, so every rule
//! about what a file may say is testable without either.
//!
//! The file is read from a user-writable subvolume by a process that is root on the host, so it
//! is treated as hostile input end to end: bounded in size, parsed as data, and every attribute
//! name checked against a grammar that cannot spell code before it is ever rendered into an
//! expression.

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const FILE_NAME: &str = "kloudlite.yaml";
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_PACKAGES: usize = 100;
pub const MAX_ATTR_LEN: usize = 64;
/// Inside the pod, where the workspace's own profile is mounted.
pub const PROFILE_MOUNT: &str = "/nix/profile";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Packages {
    pub packages: Vec<String>,
    pub nixpkgs: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FileError {
    TooLarge(usize),
    Yaml(String),
    Attr(String),
    Pin(String),
    TooMany(usize),
    Duplicate(String),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::TooLarge(n) => write!(f, "{FILE_NAME} is {n} bytes; the limit is {MAX_FILE_BYTES}"),
            FileError::Yaml(e) => write!(f, "{FILE_NAME}: {e}"),
            FileError::Attr(a) => write!(f, "{FILE_NAME}: {a:?} is not a package attribute name"),
            FileError::Pin(p) => write!(f, "{FILE_NAME}: nixpkgs must be github:NixOS/nixpkgs/<commit>, not {p:?}"),
            FileError::TooMany(n) => write!(f, "{FILE_NAME}: {n} packages; the limit is {MAX_PACKAGES}"),
            FileError::Duplicate(a) => write!(f, "{FILE_NAME}: {a:?} is listed twice"),
        }
    }
}

#[derive(Deserialize, Default)]
struct Raw {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    nixpkgs: Option<String>,
}

pub fn parse_file(bytes: &[u8]) -> Result<Packages, FileError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(FileError::TooLarge(bytes.len()));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| FileError::Yaml(e.to_string()))?;
    // Tags, anchors and aliases are the YAML features that make a document more than data, and
    // serde_yaml resolves them before we ever see the result — so they're refused by text, ahead
    // of parsing, not by inspecting the parsed tree. Every spelling of `&anchor`, `*alias` and
    // `!tag` needs one of `&`/`*`/`!` somewhere in the line; none of those characters can appear
    // in a valid attribute name or pin, so rejecting the character anywhere in the (comment-
    // stripped) text is sound for every flow/block spelling, not just anchors at a value's head.
    // ponytail: text-level scan, not the YAML event stream — it also rejects an unknown/ignored
    // key whose value merely contains one of these characters. Upgrade to a YAML parser that
    // exposes raw events (walk them and refuse Alias/Tag events specifically) if that false
    // positive on ignored keys ever matters.
    let stripped: String = text
        .lines()
        .map(|l| match l.find('#') {
            Some(i) if i == 0 || l.as_bytes()[i - 1].is_ascii_whitespace() => &l[..i],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if stripped.contains(['&', '*', '!']) {
        return Err(FileError::Yaml("tags and anchors are not allowed".into()));
    }
    let raw: Raw = if text.trim().is_empty() {
        Raw::default()
    } else {
        serde_yaml::from_str(text).map_err(|e| FileError::Yaml(e.to_string()))?
    };
    if raw.packages.len() > MAX_PACKAGES {
        return Err(FileError::TooMany(raw.packages.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for p in &raw.packages {
        validate_attr(p)?;
        if !seen.insert(p.as_str()) {
            return Err(FileError::Duplicate(p.clone()));
        }
    }
    if let Some(pin) = &raw.nixpkgs {
        validate_pin(pin)?;
    }
    Ok(Packages { packages: raw.packages, nixpkgs: raw.nixpkgs })
}

pub fn validate_attr(s: &str) -> Result<(), FileError> {
    let mut chars = s.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'));
    if ok_first && ok_rest && s.len() <= MAX_ATTR_LEN {
        Ok(())
    } else {
        Err(FileError::Attr(s.to_string()))
    }
}

fn validate_pin(p: &str) -> Result<(), FileError> {
    let rev = p.strip_prefix("github:NixOS/nixpkgs/").ok_or_else(|| FileError::Pin(p.to_string()))?;
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(FileError::Pin(p.to_string()))
    }
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
    fn a_plain_file_parses_and_unknown_keys_are_ignored() {
        let p = parse_file(b"packages:\n  - nodejs_20\n  - python3Packages.requests\nports: [3000]\n").unwrap();
        assert_eq!(p.packages, ["nodejs_20", "python3Packages.requests"]);
        assert_eq!(p.nixpkgs, None);
    }

    #[test]
    fn a_missing_packages_key_is_an_empty_list() {
        assert!(parse_file(b"name: demo\n").unwrap().packages.is_empty());
        assert!(parse_file(b"").unwrap().packages.is_empty());
    }

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
    fn the_file_is_untrusted() {
        assert!(matches!(parse_file(&vec![b' '; MAX_FILE_BYTES + 1]), Err(FileError::TooLarge(_))));
        assert!(matches!(parse_file(b"packages: !!binary abc\n"), Err(FileError::Yaml(_))));
        assert!(matches!(parse_file(b"packages: &a [hello]\nother: *a\n"), Err(FileError::Yaml(_))));
        assert!(matches!(parse_file(b"packages: [hello, hello]\n"), Err(FileError::Duplicate(_))));
        let many: Vec<String> = (0..101).map(|i| format!("p{i}")).collect();
        let yaml = format!("packages: [{}]\n", many.join(", "));
        assert!(matches!(parse_file(yaml.as_bytes()), Err(FileError::TooMany(101))));
        assert!(matches!(parse_file(b"packages: [hello]\nnixpkgs: github:evil/nixpkgs/abc\n"), Err(FileError::Pin(_))));
        let pin = "github:NixOS/nixpkgs/".to_string() + &"a".repeat(40);
        assert_eq!(parse_file(format!("packages: [hello]\nnixpkgs: {pin}\n").as_bytes()).unwrap().nixpkgs.as_deref(), Some(pin.as_str()));
    }

    #[test]
    fn flow_style_anchors_are_refused_too() {
        assert!(matches!(parse_file(b"packages: [&a hello, *a]\n"), Err(FileError::Yaml(_))));
    }

    #[test]
    fn a_comment_above_a_valid_list_is_fine() {
        let p = parse_file(b"# hi!\npackages:\n  - hello\n").unwrap();
        assert_eq!(p.packages, ["hello"]);
    }

    #[test]
    fn an_ignored_key_with_a_bang_is_still_refused_documented_behaviour() {
        assert!(matches!(parse_file(b"packages: [hello]\nnote: wow!\n"), Err(FileError::Yaml(_))));
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
