/// Identity established by a *peer*. `None` on the public listener, always.
#[derive(Clone)]
pub struct Trusted(pub Option<String>);

/// The credential inside an Authorization header of the named scheme, or `None` for another
/// scheme. Matched case-insensitively: RFC 7235 says `basic` and `Basic` are the same scheme,
/// and some proxies lowercase it.
///
/// A small duplicate of `rustic_git_storage::auth::scheme` (and `user_names` below of
/// `rustic_git_storage::auth::user_names`): that copy stays pure and `axum`-free for `storage`'s
/// own callers, while the header-parsing helpers here need `axum::http::HeaderMap` and are shared
/// by both the `api` and `registry` crates, neither of which may depend on the other.
pub fn scheme<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let (head, rest) = v.split_at_checked(name.len())?;
    (head.eq_ignore_ascii_case(name) && rest.starts_with(' ')).then(|| rest.trim_start())
}

fn user_names(user: &str, owner: &str, git_placeholder: bool) -> bool {
    user == owner || (git_placeholder && user == GIT_PLACEHOLDER)
}

/// git's placeholder username, the shape every token-based git URL uses: `https://x:<token>@host`.
/// The token IS the identity there and git has no other way to send one, so the username carries
/// no information and must not be held against the caller.
const GIT_PLACEHOLDER: &str = "x";

/// The token from a `Bearer` Authorization header.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    scheme(headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?, "Bearer")
}

/// Both halves of a `Basic` Authorization header.
pub fn basic_creds(headers: &axum::http::HeaderMap) -> Option<(String, String)> {
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

/// Cap on a single request body (compressed bytes on the wire). Axum enforces this in the
/// extractor, BEFORE the handler runs, so an unauthenticated client cannot make the server
/// buffer more than this; the git handlers apply it by hand AFTER authenticating, so that client
/// cannot make them buffer anything at all. Override with RUSTIC_GIT_MAX_BODY (bytes).
pub fn max_body() -> usize {
    std::env::var("RUSTIC_GIT_MAX_BODY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024) // 2 GiB
}
