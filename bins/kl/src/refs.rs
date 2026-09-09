//! Reference expansion: what a person types → what is pushed. The owner never types the registry
//! host or their own slug; a team member types `team/name`; anything that already names a host
//! (docker's own rule: a first segment with a `.` or a `:`) is left exactly as given.

/// `hello` → `host/owner/hello:latest`, `hello:1` → `host/owner/hello:1`,
/// `acme/hello:1` → `host/acme/hello:1`, `ghcr.io/x/y:1` → unchanged.
pub fn expand(given: &str, host: &str, owner: &str) -> String {
    let first = given.split('/').next().unwrap_or_default();
    if given.contains('/') && (first.contains('.') || first.contains(':')) {
        return given.to_string();
    }
    let path = if given.contains('/') { given.to_string() } else { format!("{owner}/{given}") };
    // A digest already pins the image; a tag is only defaulted when neither is present. The `:`
    // test looks past the last `/` so a host port never reads as a tag.
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('@') || last.contains(':') {
        format!("{host}/{path}")
    } else {
        format!("{host}/{path}:latest")
    }
}

#[cfg(test)]
mod tests {
    use super::expand;
    const H: &str = "cr.khost.dev";

    #[test]
    fn a_bare_name_gets_host_owner_and_latest() {
        assert_eq!(expand("hello", H, "alice"), "cr.khost.dev/alice/hello:latest");
    }

    #[test]
    fn a_name_with_a_tag_keeps_the_tag() {
        assert_eq!(expand("hello:1", H, "alice"), "cr.khost.dev/alice/hello:1");
    }

    #[test]
    fn an_owner_prefix_replaces_the_callers_own() {
        assert_eq!(expand("acme/hello:1", H, "alice"), "cr.khost.dev/acme/hello:1");
        assert_eq!(expand("acme/hello", H, "alice"), "cr.khost.dev/acme/hello:latest");
    }

    #[test]
    fn a_reference_that_names_a_host_is_unchanged() {
        assert_eq!(expand("ghcr.io/x/y:1", H, "alice"), "ghcr.io/x/y:1");
        assert_eq!(expand("localhost:5000/y", H, "alice"), "localhost:5000/y");
        assert_eq!(expand("cr.khost.dev/alice/hello:2", H, "alice"), "cr.khost.dev/alice/hello:2");
    }

    #[test]
    fn a_digest_reference_is_never_given_a_tag() {
        assert_eq!(expand("hello@sha256:abcd", H, "alice"), "cr.khost.dev/alice/hello@sha256:abcd");
    }
}
