//! Both credential shapes, ending at one authorization call.
//!
//! Clients that follow the spec take the Bearer challenge and fetch a scoped token from
//! `/v2/token`. Clients that do not — and every `curl` in a debugging session — send Basic
//! directly. Accepting both costs one extra branch and removes a whole class of "docker login
//! worked but push did not" reports.
use super::store::ImageExt;
use crate::Trusted;
use crate::App;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;

fn realm() -> String {
    // The externally reachable base URL. The challenge must name a URL the CLIENT can reach, not
    // this pod's address, so it is configuration rather than something derived from the request.
    std::env::var("RUSTIC_GIT_EXTERNAL_URL").unwrap_or_else(|_| "http://localhost:8080".into())
}

pub fn challenge(scope: Option<&str>) -> Response {
    challenge_for(&realm(), scope)
}

/// Refuses a `RUSTIC_GIT_EXTERNAL_URL` that cannot be a header value, so the process fails at
/// boot instead of `challenge` panicking on the first anonymous request.
pub fn check_external_url() -> crate::Result<()> {
    let base = realm();
    header::HeaderValue::from_str(&challenge_value(&base, None))
        .map(|_| ())
        .map_err(|_| crate::err(format!("RUSTIC_GIT_EXTERNAL_URL is not a valid header value: {base:?}")))
}

fn challenge_value(base: &str, scope: Option<&str>) -> String {
    let host = base.split("://").nth(1).unwrap_or("registry");
    let mut v = format!("Bearer realm=\"{base}/v2/token\",service=\"{host}\"");
    if let Some(s) = scope {
        v.push_str(&format!(",scope=\"{s}\""));
    }
    v
}

fn challenge_for(base: &str, scope: Option<&str>) -> Response {
    let mut r = crate::oci_err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required");
    // `check_external_url` ran at boot, so this only fails on a scope with control characters —
    // and a challenge without a realm is a refusal, never a panic.
    if let Ok(v) = header::HeaderValue::from_str(&challenge_value(base, scope)) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
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
    if crate::httpauth::scheme(v, "Basic").is_some() {
        let Some(token) = crate::httpauth::basic_token(headers) else { return Err(challenge(None)) };
        // The token is the secret, but the username must be the owner it belongs to: a credential
        // whose halves disagree did not verify, and a leaked token must not work under any name.
        // No placeholder here — unlike git, `docker login` always has a real username to send.
        return match app.store.owner_for_token(&token).await {
            Ok(Some(o)) if crate::httpauth::basic_user_names(headers, &o, false) => Ok(Some(o)),
            Ok(_) => Err(challenge(None)),
            Err(e) => Err(crate::oci_internal(e)),
        };
    }
    if let Some(jwt) = crate::httpauth::scheme(v, "Bearer") {
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
        Some(_) => crate::oci_err(StatusCode::FORBIDDEN, "DENIED", "insufficient scope"),
    })
}

#[cfg(test)]
mod challenge_tests {
    use super::*;

    #[test]
    fn a_malformed_external_url_never_panics_the_challenge() {
        let r = challenge_for("http://bad\nhost", Some("repository:a/b:pull"));
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(r.headers().get(header::WWW_AUTHENTICATE).is_none());
        let ok = challenge_for("https://cr.example", Some("repository:a/b:pull"));
        assert_eq!(
            ok.headers().get(header::WWW_AUTHENTICATE).unwrap().to_str().unwrap(),
            "Bearer realm=\"https://cr.example/v2/token\",service=\"cr.example\",scope=\"repository:a/b:pull\""
        );
    }
}
