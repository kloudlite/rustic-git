//! Pin every service image to its digest when an environment's services are written.
//!
//! Incident 2026-10-10: a session rebuilt `chat-api:v1` and redeployed the same tag; the node kept
//! its cached copy, so the old code ran and the session debugged a build it never deployed. A tag
//! is a name for "whatever was pushed last"; the spec should say which bytes run. So create,
//! restore and the services patch rewrite `repo:tag` to `repo:tag@sha256:…` (the tag stays for the
//! reader; the runtime pulls by digest). An image already carrying `@` is left as written.
//!
//! Best effort by design: a registry that is down, slow or private to someone else leaves the tag
//! as written (`image.unpinned` in the log) and the service pull policy `Always` still fetches
//! the newest bytes. Refusing the write over a lookup would block every deploy on Docker Hub.

use super::ApiState;
use crate::model::Service;
use std::time::Duration;

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
const LOOKUP: Duration = Duration::from_secs(5);

/// `(registry host for the API, repository, tag)`, Docker's defaulting rules: no host means
/// Docker Hub, a one-part Hub name is under `library/`, no tag means `latest`. `None` for an image
/// already pinned (`@`) or empty.
pub(crate) fn split_ref(image: &str) -> Option<(String, String, String)> {
    if image.is_empty() || image.contains('@') {
        return None;
    }
    let (first, rest) = image.split_once('/').unwrap_or(("", image));
    let (host, path) = if !first.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost") {
        (first.to_string(), rest.to_string())
    } else {
        let path = if first.is_empty() { format!("library/{image}") } else { image.to_string() };
        ("registry-1.docker.io".to_string(), path)
    };
    // The tag is after the last ':' of the last path segment; a ':' before a '/' was a port.
    let (repo, tag) = match path.rsplit_once(':') {
        Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
        _ => (path, "latest".to_string()),
    };
    Some((host, repo, tag))
}

/// `Bearer realm="…",service="…",scope="…"` → the token URL; only the anonymous token dance, the
/// one Docker Hub and most public registries answer.
fn token_url(challenge: &str, repo: &str) -> Option<String> {
    let params = challenge.strip_prefix("Bearer ")?;
    let field = |k: &str| {
        params.split(',').find_map(|p| p.trim().strip_prefix(&format!("{k}=")).map(|v| v.trim_matches('"').to_string()))
    };
    let realm = field("realm")?;
    let mut url = reqwest::Url::parse(&realm).ok()?;
    if let Some(svc) = field("service") {
        url.query_pairs_mut().append_pair("service", &svc);
    }
    url.query_pairs_mut().append_pair("scope", &field("scope").unwrap_or_else(|| format!("repository:{repo}:pull")));
    Some(url.to_string())
}

async fn digest_of(http: &reqwest::Client, host: &str, repo: &str, tag: &str, basic: Option<(&str, &str)>) -> Result<String, String> {
    let url = format!("https://{host}/v2/{repo}/manifests/{tag}");
    let head = |auth: Option<String>, basic: Option<(&str, &str)>| {
        let mut r = http.head(&url).header("Accept", ACCEPT);
        if let Some((u, p)) = basic {
            r = r.basic_auth(u, Some(p));
        }
        if let Some(t) = auth {
            r = r.bearer_auth(t);
        }
        r.send()
    };
    let mut resp = head(None, basic).await.map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED && basic.is_none() {
        let challenge = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default();
        let turl = token_url(challenge, repo).ok_or("no bearer challenge")?;
        let body: serde_json::Value = http.get(turl).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
        let token = body.get("token").or_else(|| body.get("access_token")).and_then(|t| t.as_str()).ok_or("no token")?;
        resp = head(Some(token.to_string()), None).await.map_err(|e| e.to_string())?;
    }
    if !resp.status().is_success() {
        return Err(format!("manifest {}", resp.status()));
    }
    let d = resp.headers().get("docker-content-digest").and_then(|v| v.to_str().ok()).ok_or("no Docker-Content-Digest")?;
    if !d.starts_with("sha256:") {
        return Err(format!("unexpected digest {d}"));
    }
    Ok(d.to_string())
}

/// Rewrite each unpinned image of `services` to `image@digest`. Own-registry images are read as
/// `owner` (the same short-lived credential the `registry-pull` Secret carries), others
/// anonymously; the lookups run together, each bounded by `LOOKUP`.
pub(crate) async fn pin_images(s: &ApiState, owner: &str, services: &mut [Service]) {
    let own = std::env::var("KLOUDLITE_REGISTRY_HOST").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
    let token = own.as_ref().and_then(|_| s.jwt.mint_registry(owner, "*", 300).ok());
    let Ok(http) = reqwest::Client::builder().timeout(LOOKUP).build() else { return };
    let lookups = services.iter().map(|svc| {
        let (http, own, token) = (&http, own.as_deref(), token.as_deref());
        async move {
            let (host, repo, tag) = split_ref(&svc.image)?;
            let basic = (Some(host.as_str()) == own).then_some(token).flatten().map(|t| (owner, t));
            match digest_of(http, &host, &repo, &tag, basic).await {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!(image = %svc.image, error = %e, "image.unpinned");
                    None
                }
            }
        }
    });
    let digests = futures::future::join_all(lookups).await;
    for (svc, d) in services.iter_mut().zip(digests) {
        if let Some(d) = d {
            tracing::info!(image = %svc.image, digest = %d, "image.pinned");
            svc.image = format!("{}@{d}", svc.image);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(i: &str) -> Option<(String, String, String)> {
        split_ref(i)
    }
    fn t(h: &str, p: &str, g: &str) -> Option<(String, String, String)> {
        Some((h.into(), p.into(), g.into()))
    }

    #[test]
    fn a_reference_splits_by_dockers_defaulting_rules() {
        assert_eq!(r("nats:2.10"), t("registry-1.docker.io", "library/nats", "2.10"));
        assert_eq!(r("mongo"), t("registry-1.docker.io", "library/mongo", "latest"));
        assert_eq!(r("bitnami/redis:7"), t("registry-1.docker.io", "bitnami/redis", "7"));
        assert_eq!(r("cr.khost.dev/karthik1729/chat-api:v1"), t("cr.khost.dev", "karthik1729/chat-api", "v1"));
        assert_eq!(r("localhost:5000/app"), t("localhost:5000", "app", "latest"), "a port is not a tag");
        assert_eq!(r("ghcr.io/o/a/b:1.2"), t("ghcr.io", "o/a/b", "1.2"));
    }

    #[test]
    fn a_pinned_or_empty_image_is_left_as_written() {
        assert_eq!(r("nats:2.10@sha256:abc"), None);
        assert_eq!(r(""), None);
    }

    #[test]
    fn the_token_url_carries_service_and_scope() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nats:pull""#;
        assert_eq!(
            token_url(c, "library/nats").as_deref(),
            Some("https://auth.docker.io/token?service=registry.docker.io&scope=repository%3Alibrary%2Fnats%3Apull")
        );
        assert_eq!(token_url("Basic realm=\"x\"", "r"), None);
    }
}
