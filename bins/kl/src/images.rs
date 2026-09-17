//! `kl container images` — what the registry already holds, with the same credential
//! `docker-credential-kl` hands buildx: the `registry-token` file beside `workspace-token`, Basic,
//! under the owner's own name. No `/v1` call and no api credential, exactly like `build` and
//! `push`.
//!
//! `/v2/_catalog` is scoped to the CREDENTIAL's owner (`registry::routes::catalog`: it lists
//! `image_listing` for the authenticated caller and nothing else), so `--owner` narrows what this
//! workspace's credential can already see rather than reaching somebody else's catalog.
//! ponytail: a team's catalog needs a credential minted for the team; the upgrade path is a
//! `/v2/_catalog?owner=` the registry gates with `may_act`, and this says so rather than
//! pretending an empty answer means an empty registry.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Same mount as `workspace-token` (`USER_KEY_PATH` in `crates/workspaces/src/k8s/secrets.rs`),
/// and the same file `docker-credential-kl` reads.
pub const TOKEN_PATH: &str = "/etc/kloudlite/ssh/registry-token";

pub struct Registry {
    base: String,
    owner: String,
    token_path: PathBuf,
    agent: ureq::Agent,
}

impl Registry {
    pub fn from_env() -> Result<Self, String> {
        let host = crate::env("KL_REGISTRY_HOST")?;
        Ok(Self::new(format!("https://{}", host.trim_end_matches('/')), crate::env("KL_OWNER")?, PathBuf::from(TOKEN_PATH)))
    }

    pub fn new(base: impl Into<String>, owner: impl Into<String>, token_path: PathBuf) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { base: base.into().trim_end_matches('/').to_string(), owner: owner.into(), token_path, agent }
    }

    /// The Basic header the registry expects: the username must NAME the credential's owner
    /// (`httpx::basic_user_names`), so a token presented under any other name is refused — which
    /// is why `--owner` is a filter here and never a second identity.
    fn authorization(&self) -> Result<String, String> {
        let token = std::fs::read_to_string(&self.token_path)
            .map(|t| t.trim().to_string())
            .map_err(|_| format!("no registry credential at {} — kl runs inside a kloudlite workspace", self.token_path.display()))?;
        Ok(format!("Basic {}", basic(&self.owner, &token)))
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let auth = self.authorization()?;
        let mut res = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("authorization", &auth)
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().map_err(|e| format!("GET {path}: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(refusal(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| format!("GET {path}: the registry answered something that is not json: {e}"))
    }
}

/// `user:token`, base64 — docker's own encoding of the pair, done here because nothing else in
/// this binary speaks the registry's wire.
fn basic(user: &str, token: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))
}

/// The OCI envelope's own sentence when there is one — `{"errors":[{"message": …}]}` — else the
/// status. The same choice `api::failure` makes, for the same reason: the message is the product.
fn refusal(status: u16, body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["errors"][0]["message"].as_str().map(str::to_string))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| format!("HTTP {status}"))
}

/// The repositories in a `_catalog` answer, under `owner` and in the order the registry gave them.
/// A name that does not carry the owner's prefix is another owner's and is dropped rather than
/// printed under a heading it does not belong to.
fn repositories(catalog: &Value, owner: &str) -> Vec<String> {
    let prefix = format!("{owner}/");
    catalog["repositories"]
        .as_array()
        .map(|rs| rs.iter().filter_map(|r| r.as_str()).filter(|r| r.starts_with(&prefix)).map(str::to_string).collect())
        .unwrap_or_default()
}

/// A `tags/list` answer. Absent and null both mean "no tags", which is a real state: an image
/// whose only tag was deleted still has a repository until the sweep collects it.
fn tags(list: &Value) -> Vec<String> {
    list["tags"].as_array().map(|ts| ts.iter().filter_map(|t| t.as_str()).map(str::to_string).collect()).unwrap_or_default()
}

/// One line per image: the reference a person would pull, then its tags.
fn line(host: &str, repo: &str, tags: &[String]) -> String {
    match tags.is_empty() {
        true => format!("{host}/{repo}\t(no tags)"),
        false => format!("{host}/{repo}\t{}", tags.join(" ")),
    }
}

/// `kl container images [--owner <slug>]`.
pub fn list(reg: &Registry, host: &str, owner: Option<&str>) -> Result<(), String> {
    let owner = owner.unwrap_or(&reg.owner);
    let catalog = reg.get("/v2/_catalog")?;
    let repos = repositories(&catalog, owner);
    if repos.is_empty() {
        // Said rather than printed as nothing: an empty answer under another owner means this
        // workspace's credential, not the registry, is what has nothing to show.
        if owner != reg.owner {
            return Err(format!(
                "no images under {owner} — this workspace's registry credential is {}'s, and the catalog is per credential",
                reg.owner
            ));
        }
        println!("no images");
        return Ok(());
    }
    for repo in repos {
        let tags = match reg.get(&format!("/v2/{repo}/tags/list")) {
            Ok(v) => tags(&v),
            // One unreadable repository must not lose the rest of the listing.
            Err(e) => {
                eprintln!("kl: {repo}: {e}");
                continue;
            }
        };
        println!("{}", line(host, &repo, &tags));
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basic_header_is_the_owner_and_the_token() {
        // What `docker-credential-kl` hands docker, encoded as docker encodes it.
        assert_eq!(basic("alice", "tok"), "YWxpY2U6dG9r");
    }

    #[test]
    fn only_this_owners_repositories_are_listed() {
        let catalog = serde_json::json!({"repositories": ["alice/web", "alice/api", "bob/web", 7]});
        assert_eq!(repositories(&catalog, "alice"), ["alice/web", "alice/api"]);
        assert!(repositories(&catalog, "carol").is_empty());
        // A prefix must be a whole segment: `alice-corp` is not `alice`.
        assert!(repositories(&serde_json::json!({"repositories": ["alice-corp/web"]}), "alice").is_empty());
        assert!(repositories(&serde_json::json!({}), "alice").is_empty());
    }

    #[test]
    fn tags_absent_or_null_is_no_tags_not_an_error() {
        assert_eq!(tags(&serde_json::json!({"name": "alice/web", "tags": ["1", "latest"]})), ["1", "latest"]);
        assert!(tags(&serde_json::json!({"name": "alice/web", "tags": null})).is_empty());
        assert!(tags(&serde_json::json!({})).is_empty());
        assert_eq!(line("reg.example", "alice/web", &[]), "reg.example/alice/web\t(no tags)");
        assert_eq!(
            line("reg.example", "alice/web", &["1".into(), "latest".into()]),
            "reg.example/alice/web\t1 latest"
        );
    }

    #[test]
    fn a_refusal_is_the_registrys_own_sentence() {
        assert_eq!(refusal(401, r#"{"errors":[{"code":"UNAUTHORIZED","message":"auth required"}]}"#), "auth required");
        assert_eq!(refusal(500, "not json"), "HTTP 500");
        assert_eq!(refusal(403, r#"{"errors":[]}"#), "HTTP 403");
    }

    #[test]
    fn missing_token_file_never_reaches_the_wire() {
        let r = Registry::new("http://127.0.0.1:1", "alice", PathBuf::from("/nonexistent/registry-token"));
        assert!(r.get("/v2/_catalog").unwrap_err().starts_with("no registry credential at"));
    }
}
