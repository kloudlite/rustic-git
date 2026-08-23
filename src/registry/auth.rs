//! Both credential shapes, ending at one authorization call.
//!
//! Clients that follow the spec take the Bearer challenge and fetch a scoped token from
//! `/v2/token`. Clients that do not — and every `curl` in a debugging session — send Basic
//! directly. Accepting both costs one extra branch and removes a whole class of "docker login
//! worked but push did not" reports.
use crate::http::Trusted;
use crate::App;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use base64::Engine;

fn realm() -> String {
    // The externally reachable base URL. The challenge must name a URL the CLIENT can reach, not
    // this pod's address, so it is configuration rather than something derived from the request.
    std::env::var("RUSTIC_GIT_EXTERNAL_URL").unwrap_or_else(|_| "http://localhost:8080".into())
}

pub fn challenge(scope: Option<&str>) -> Response {
    let base = realm();
    let host = base.split("://").nth(1).unwrap_or("registry").to_string();
    let mut v = format!("Bearer realm=\"{base}/v2/token\",service=\"{host}\"");
    if let Some(s) = scope {
        v.push_str(&format!(",scope=\"{s}\""));
    }
    let mut r = crate::registry::oci_err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required");
    r.headers_mut().insert(header::WWW_AUTHENTICATE, v.parse().unwrap());
    r
}

/// The credential after an auth scheme, matched case-insensitively: RFC 7235 says `basic` and
/// `Basic` are the same scheme, and some proxies lowercase it.
fn scheme<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let (head, rest) = v.split_at_checked(name.len())?;
    (head.eq_ignore_ascii_case(name) && rest.starts_with(' ')).then(|| rest.trim_start())
}

/// The authenticated owner, or `None` for an anonymous caller. `Err` is a response to return
/// as-is: a credential that was PRESENTED and did not verify is a refusal, not anonymity.
pub async fn caller(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
) -> Result<Option<String>, Response> {
    // A peer already authenticated this client; `trust_peer` checked the shared secret.
    if let Some(o) = trusted.0.clone() {
        return Ok(Some(o));
    }
    let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    if let Some(b64) = scheme(v, "Basic") {
        let cred = base64::engine::general_purpose::STANDARD
            .decode(b64).ok()
            .and_then(|d| String::from_utf8(d).ok())
            .and_then(|s| s.split_once(':').map(|(u, p)| (u.to_string(), p.to_string())));
        let Some((user, token)) = cred else { return Err(challenge(None)) };
        // The token is the secret, but the username must be the owner it belongs to: a credential
        // whose halves disagree did not verify, and a leaked token must not work under any name.
        return match app.store.owner_for_token(&token).await {
            Ok(Some(o)) if o == user => Ok(Some(o)),
            Ok(_) => Err(challenge(None)),
            Err(e) => Err(crate::registry::oci_internal(e)),
        };
    }
    if let Some(jwt) = scheme(v, "Bearer") {
        use super::routes::RegistryToken;
        return match super::routes::verify_registry_token(&app.jwt, jwt) {
            RegistryToken::Owner(o) => Ok(Some(o)),
            // Verified as ours, but minted for the anonymous caller: not a refusal. This is the
            // exact token a spec-following client gets from `/v2/token` before an anonymous pull
            // of a public image, so it must fall through to anonymous-continue, not a challenge.
            RegistryToken::Anonymous => Ok(None),
            RegistryToken::Invalid => Err(challenge(None)),
        };
    }
    Err(challenge(None))
}

/// Authorize a caller against one image. `write` is false for pulls.
///
/// Anonymous on a private image gets the CHALLENGE (so the client knows to log in); an
/// authenticated stranger gets DENIED (so it knows logging in again will not help).
pub async fn allow(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<Option<String>, Response> {
    let who = caller(app, trusted, headers).await?;
    if who.as_deref() == Some(owner) {
        return Ok(who);
    }
    let public = !write && app.store.image_is_public(owner, name).await.unwrap_or(false);
    if public {
        return Ok(who);
    }
    let scope = format!("repository:{owner}/{name}:{}", if write { "pull,push" } else { "pull" });
    Err(match who {
        None => challenge(Some(&scope)),
        Some(_) => crate::registry::oci_err(StatusCode::FORBIDDEN, "DENIED", "insufficient scope"),
    })
}
