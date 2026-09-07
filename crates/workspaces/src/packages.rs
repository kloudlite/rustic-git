//! The package list a workspace declares (`spec.packages` on its CRD), and everything the
//! reconciler needs derived from it. Pure on purpose: this module never touches the disk or Nix,
//! so every rule about what a list may say is testable without either.
//!
//! The list arrives from the API — which writes `spec.packages` — but the CR itself is not a
//! trust boundary the API alone controls: any principal with write access to the object (a
//! restored backup, a migration, `kubectl edit`) can put an arbitrary list there. So the same
//! grammar is checked twice: once by the API before it writes, again by the reconciler before it
//! ever renders a name into a Nix expression.

use crate::crd::Lock;
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
    /// A lock that cannot be rendered: a store path, revision or attribute that does not match
    /// the shape the expression may quote. The api writes locks, but the CR is not a trust
    /// boundary the api alone controls — see the module doc.
    Lock(String),
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
            PackageError::Lock(e) => write!(f, "{e} is not a lock this node can build"),
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

/// A store path exactly as nix writes one: `/nix/store/<32 base32 chars>-<name>`. Checked before
/// the path is quoted into an expression — `builtins.storePath` takes a string, and a string is
/// the one place in a Nix expression arbitrary bytes could become something other than a path.
fn valid_store_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/nix/store/") else { return false };
    let Some((hash, name)) = rest.split_once('-') else { return false };
    hash.len() == 32
        && hash.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"+._?=-".contains(&b))
}

/// A full nixpkgs revision. `pub` because `Nix::eval_out_path` interpolates one too.
pub fn valid_rev(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What a lock contributes to the hash and to the expression: the store path it pins, or the
/// revision and attribute to evaluate when the index gave no path (a mirror lock).
fn lock_key(l: &Lock) -> String {
    if l.store_path.is_empty() {
        format!("{}#{}", l.rev, l.attr_path)
    } else {
        l.store_path.clone()
    }
}

/// What the profile on disk IS: the pin, the sorted bare list, and what every lock resolved to.
/// Sorted so a reordered file is not a rebuild; pinned so a rolled nixpkgs is; keyed on the lock's
/// OUTPUT (a store path, or `rev#attr`) so re-resolving `nodejs@20` to a new version rebuilds and
/// re-resolving it to the same one does not.
pub fn hash(pin: &str, bare: &[String], locks: &[Lock]) -> String {
    let mut sorted: Vec<&str> = bare.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut keys: Vec<String> = locks.iter().map(|l| format!("{}={}", l.entry, lock_key(l))).collect();
    keys.sort_unstable();
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    for p in sorted {
        h.update(b"\n");
        h.update(p.as_bytes());
    }
    for k in keys {
        h.update(b"\n@");
        h.update(k.as_bytes());
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
/// A locked entry is NOT `pkgs.<attr>`: the pin has whatever version it has. A lock with a store
/// path is taken verbatim from the binary cache (`builtins.storePath`, nothing to evaluate or
/// build); a mirror lock — no store path — evaluates the attribute at the revision the index named.
/// Every one of the three quoted fields is validated first: `Err` rather than a rendered
/// expression, because a name that reaches nix has already escaped.
pub fn expression(pin: &str, bare: &[String], locks: &[Lock]) -> Result<String, PackageError> {
    let mut paths: Vec<String> = bare.iter().map(|p| format!("pkgs.{p}")).collect();
    for l in locks {
        if l.store_path.is_empty() {
            if !valid_rev(&l.rev) {
                return Err(PackageError::Lock(format!("{}: {:?} is not a nixpkgs revision", l.entry, l.rev)));
            }
            validate_attr(&l.attr_path)?;
            paths.push(format!(
                "(import (builtins.getFlake \"github:NixOS/nixpkgs/{}\") {{ }}).{}",
                l.rev, l.attr_path
            ));
        } else {
            if !valid_store_path(&l.store_path) {
                return Err(PackageError::Lock(format!("{}: {:?} is not a store path", l.entry, l.store_path)));
            }
            // Parenthesised: a bare `builtins.storePath "..."` inside a list literal is TWO
            // elements, and nix would take the function itself as one of the profile's paths.
            paths.push(format!("(builtins.storePath \"{}\")", l.store_path));
        }
    }
    Ok(format!(
        "let pkgs = import (builtins.getFlake \"{pin}\") {{ }}; in pkgs.buildEnv {{ name = \"kloudlite-workspace-env\"; paths = [ {} ]; }}",
        paths.join(" ")
    ))
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

    fn lock(entry: &str, store_path: &str) -> Lock {
        Lock {
            entry: entry.into(),
            version: "20.20.2".into(),
            attr_path: "nodejs_20".into(),
            rev: "a".repeat(40),
            store_path: store_path.into(),
            resolved_at: "2026-09-08T00:00:00Z".into(),
            source: crate::crd::LockSource::Nixhub,
        }
    }

    const STORE: &str = "/nix/store/00000000000000000000000000000000-nodejs-20.20.2";

    /// A re-resolve that lands on the same output must NOT rebuild; one that lands elsewhere must.
    #[test]
    fn the_hash_covers_what_each_lock_resolved_to() {
        let pin = "github:NixOS/nixpkgs/aaaa";
        let same = hash(pin, &["jq".into()], &[lock("nodejs@20", STORE)]);
        assert_eq!(same, hash(pin, &["jq".into()], &[lock("nodejs@20", STORE)]));
        let other = hash(pin, &["jq".into()], &[lock("nodejs@20", &STORE.replace("00000", "11111"))]);
        assert_ne!(same, other, "a new store path is a new profile");
        // A mirror lock has no store path, so `rev#attr` is what it contributes.
        let mut mirror = lock("nodejs@20", "");
        let a = hash(pin, &[], std::slice::from_ref(&mirror));
        mirror.rev = "b".repeat(40);
        assert_ne!(a, hash(pin, &[], &[mirror]));
        assert_ne!(same, hash(pin, &["jq".into()], &[]), "a lock is part of the inputs");
    }

    #[test]
    fn the_hash_is_order_independent_and_pin_sensitive() {
        let a = hash("github:NixOS/nixpkgs/aaaa", &["go".into(), "jq".into()], &[]);
        let b = hash("github:NixOS/nixpkgs/aaaa", &["jq".into(), "go".into()], &[]);
        let c = hash("github:NixOS/nixpkgs/bbbb", &["go".into(), "jq".into()], &[]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn the_expression_is_a_list_literal_never_interpolated_text() {
        let e = expression("github:NixOS/nixpkgs/aaaa", &["go".into(), "python3Packages.requests".into()], &[]).unwrap();
        assert_eq!(
            e,
            "let pkgs = import (builtins.getFlake \"github:NixOS/nixpkgs/aaaa\") { }; in pkgs.buildEnv { name = \"kloudlite-workspace-env\"; paths = [ pkgs.go pkgs.python3Packages.requests ]; }"
        );
        let empty = expression("github:NixOS/nixpkgs/aaaa", &[], &[]).unwrap();
        assert!(empty.contains("paths = [  ];"));
    }

    /// The two lock shapes, and the refusal that keeps either from becoming a quoting hole.
    #[test]
    fn a_lock_is_substituted_or_evaluated_never_taken_from_the_pin() {
        let cached = expression("github:NixOS/nixpkgs/aaaa", &["jq".into()], &[lock("nodejs@20", STORE)]).unwrap();
        assert!(cached.contains(&format!("paths = [ pkgs.jq (builtins.storePath \"{STORE}\") ]")), "{cached}");

        let mirror = expression("github:NixOS/nixpkgs/aaaa", &[], &[lock("nodejs@20", "")]).unwrap();
        let rev = "a".repeat(40);
        assert!(
            mirror.contains(&format!("(import (builtins.getFlake \"github:NixOS/nixpkgs/{rev}\") {{ }}).nodejs_20")),
            "{mirror}"
        );

        for bad in [
            "/nix/store/../../etc/passwd",
            "/nix/store/00000000000000000000000000000000-a\" ]; x = builtins.exec [\"id\"]; y = [ \"",
            "/etc/passwd",
            "/nix/store/short-nodejs",
            "",
        ] {
            let mut l = lock("nodejs@20", bad);
            if bad.is_empty() {
                // An empty store path is a MIRROR lock — refuse it on the revision instead.
                l.rev = "not-a-revision".into();
            }
            assert!(expression("github:NixOS/nixpkgs/aaaa", &[], &[l]).is_err(), "{bad:?} must be refused");
        }
        let mut bad_attr = lock("nodejs@20", "");
        bad_attr.attr_path = "nodejs; rm -rf /".into();
        assert!(expression("github:NixOS/nixpkgs/aaaa", &[], &[bad_attr]).is_err());
    }

    /// Two workspaces with the same inputs must produce the SAME derivation, or the store cannot
    /// share it and a clone rebuilds what its source already has.
    #[test]
    fn the_expression_does_not_depend_on_which_workspace_asked() {
        let a = expression("github:NixOS/nixpkgs/aaaa", &["go".into()], &[]).unwrap();
        let b = expression("github:NixOS/nixpkgs/aaaa", &["go".into()], &[]).unwrap();
        assert_eq!(a, b);
        assert!(!a.contains("ws-"), "the workspace id must not reach the derivation name: {a}");
    }

    #[test]
    fn path_env_prepends_the_profile_and_falls_back_to_a_sane_default() {
        assert_eq!(path_env(Some("/opt/bin:/usr/bin")), "/nix/profile/current/bin:/opt/bin:/usr/bin");
        assert_eq!(path_env(None), "/nix/profile/current/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
}
