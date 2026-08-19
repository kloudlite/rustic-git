//! The read API's own process. It holds no repository state: it authenticates, consults the
//! cache, and on a miss asks the git fleet, whose routing already knows which node owns what.
//!
//! Browse routes live on the git nodes' PEER listener only, so `upstream` is the peer Service and
//! every forwarded request carries the peer secret plus, when the caller is authenticated, the
//! owner header — exactly the identity a forwarding node presents, so upstream authorizes this
//! the way it authorizes a peer.

use crate::cache::Cache;
use crate::store::Store;
use crate::Result;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine;
use std::sync::Arc;

/// How long each kind of answer is kept. Only `refs` can go stale; the rest are keyed by an
/// object id and are true forever, so their TTL is an eviction hint rather than a correctness one.
const TTL_REFS: u64 = 5;
const TTL_IMMUTABLE: u64 = 7 * 24 * 3600;
const TTL_META: u64 = 30;
const MAX_CACHED_BODY: usize = 1 << 20;
/// Hard ceiling on what is read from a git node. `MAX_CACHED_BODY` gates only what is KEPT; a
/// reply is buffered whole before it is answered, so without this one bad node is the same memory
/// cliff the push path hit. Comfortably above the largest browse answer (a 1 MiB inline blob).
const MAX_BODY: usize = 8 << 20;
/// A hanging git node must not hang every api request. `proxy::LEADER_TIMEOUT` is the precedent;
/// browse answers come off an already-open odb, so they are not slower than a lease call.
pub const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The visibility flag, cached apart from the answers it guards: it is what lets a hit be served
/// without asking a git node who may read this repo.
/// `%` is always escaped in a suffix, so `%00` is a byte sequence no path can produce: the
/// visibility flag can never collide with a cached answer (`/api/o/n/meta` would otherwise key on
/// exactly this).
pub const META: &str = "%00meta";

pub struct Api {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    /// `None` when no database is configured: the browse routes still answer, and
    /// only the team routes report that they are unavailable. A missing database
    /// must not take down reads that never needed it.
    pub directory: Option<Arc<crate::directory::Directory>>,
    /// Mints and verifies identity tokens. `None` leaves only the peer-header
    /// path, which is enough for internal calls but cannot issue a session.
    pub jwt: Option<Arc<crate::jwt::Jwt>>,
    /// Base URL of the git peer Service, e.g. `http://rustic-git:8081`.
    pub upstream: String,
    pub secret: String,
    pub client: reqwest::Client,
}

pub async fn serve(
    store: Arc<Store>,
    cache: Arc<Cache>,
    directory: Option<Arc<crate::directory::Directory>>,
    jwt: Option<Arc<crate::jwt::Jwt>>,
    upstream: String,
    secret: String,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    let api = Arc::new(Api {
        store,
        cache,
        directory,
        jwt,
        upstream: upstream.trim_end_matches('/').to_string(),
        secret,
        client: reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .unwrap_or_default(),
    });
    let app = Router::new()
        // Ahead of the fallback: `/healthz` is not a repo path and must never reach `handle`,
        // which would treat it as `/api/{owner}/{name}/...` and 404.
        .route("/healthz", axum::routing::get(|| async { StatusCode::OK }))
        // Team routes sit under /v1/, which `api_route` never parses, so they can
        // never collide with a repo path. Registered before the fallback because
        // the fallback is GET-only and would swallow the POST as a 405.
        .route("/v1/teams", axum::routing::post(create_team).get(list_teams))
        // Sign-in calls this. It is an upsert, not a create: the web app cannot
        // know whether this is someone's first visit, and should not have to.
        .route("/v1/users", axum::routing::post(upsert_user))
        // Picking a handle. Separate from sign-in because it happens once, later,
        // and can fail in a way sign-in must not: the handle may be taken.
        .route("/v1/users/username", axum::routing::post(claim_username))
        // Creating a repo. The api tier owns the question the git fleet cannot
        // answer — whether this person may create under this owner — and then
        // forwards to the node that will serve the repo.
        .route("/v1/repos", axum::routing::post(create_repo).get(list_repos))
        // A repo's own settings. `{owner}/{name}` rather than a flat id so the
        // path reads as the repo it is about.
        .route(
            "/v1/repos/{owner}/{name}",
            axum::routing::patch(update_repo).delete(delete_repo),
        )
        .route(
            "/v1/repos/{owner}/{name}/protection",
            axum::routing::get(list_protection).post(set_protection),
        )
        // Credentials: what a person uses to clone and push. The secret is written
        // to the object store the fleet authenticates against; only its metadata
        // is recorded here, so this is the one place that can list or revoke.
        .route("/v1/tokens", axum::routing::post(create_token).get(list_tokens))
        .route("/v1/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/v1/keys", axum::routing::post(add_key).get(list_keys))
        .route("/v1/keys/{id}", axum::routing::delete(remove_key))
        // Passkeys. Registration and listing are the signed-in person's; the
        // lookup is not — a sign-in has no session yet, which is the point.
        .route("/v1/passkeys", axum::routing::post(add_passkey).get(list_passkeys))
        .route("/v1/passkeys/{id}", axum::routing::delete(remove_passkey))
        .route("/v1/passkeys/lookup", axum::routing::post(lookup_passkey))
        .route("/v1/passkeys/{id}/used", axum::routing::post(passkey_used))
        // GET only. These are read-only views, and forwarding a POST as a GET (which is what
        // `any` did) would let a method the fleet never sees drive the cache.
        .fallback(axum::routing::get(handle))
        .with_state(api);
    axum::serve(listener, app).await?;
    Ok(())
}

/// One parsed request. Authorization, the cache key and the upstream URL all come from THIS —
/// never from the raw URI. Deriving them from different strings is how `..` in a path authorizes
/// one repo and reads another: `Url::parse` removes dot segments, a hand-rolled split does not.
struct Parsed {
    /// `owner/name` — what the visibility check and the cache are keyed on.
    repo: String,
    /// The cache suffix. Injective: a segment's `%` and `:` are escaped, so no two distinct
    /// paths can collide on one entry.
    suffix: String,
    /// The path forwarded upstream, rebuilt from the same segments.
    path: String,
}

/// Escape everything that carries meaning in the suffix grammar, whose separators are `:`
/// between segments and `?` before the query. Segments are DECODED before this runs, so both can
/// reach it as ordinary bytes — an unescaped `?` would make `/tree/a%3Fpage=2` and `/tree/a` with
/// `?page=2` one cache entry. `%` goes first, or the escapes this adds would themselves be
/// re-escaped.
fn escape(seg: &str) -> String {
    seg.replace('%', "%25")
        .replace(':', "%3A")
        .replace('?', "%3F")
}

/// Percent-decode one path segment, exactly once. `None` for a malformed escape or non-UTF-8:
/// nothing legitimate here is either, and guessing is how a decoder becomes a second parser.
fn decode(seg: &str) -> Option<String> {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            // `from_str_radix` accepts a leading `+`, so the digits are checked first.
            if !hex.iter().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Re-encode a decoded segment for the forwarded path: everything outside the unreserved set
/// becomes `%XX`, so the bytes sent upstream are the bytes that were validated — no `/`, no `\`,
/// and no second spelling of a dot segment can survive.
fn encode(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `/api/{owner}/{name}/{tail...}` with a query.
///
/// Every segment is DECODED first and judged on the decoded value: comparing raw text against a
/// list of spellings does not work, because `url::Url::parse` (inside reqwest) strips `%2e%2e`,
/// `%2E.` and friends as well as a literal `..`, and turns `\` into `/` — so a path that looked
/// harmless here would be shortened into a different repo before it reached a git node. Empty,
/// `.`, `..`, or anything containing a separator is refused; nothing is ever normalised.
fn split_api_path(path: &str, query: Option<&str>) -> Option<Parsed> {
    let rest = path.trim_start_matches('/').strip_prefix("api/")?;
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.len() < 3 {
        return None;
    }
    let segs: Vec<String> = segs.iter().map(|s| decode(s)).collect::<Option<_>>()?;
    if segs.iter().any(|s| {
        s.is_empty() || s == "." || s == ".." || s.contains('/') || s.contains('\\') || s.contains('#')
    }) {
        return None;
    }
    let (owner, name, tail) = (&segs[0], &segs[1], &segs[2..]);
    // The repo half of the key must be a real repo name, or `alice/web:c` (invalid, always a 404
    // upstream) keys identically to `alice/web` with tail `c`. Nothing is cached under a 404 today,
    // so this closes the class rather than a live bug — and saves the upstream round trip.
    if !crate::store::valid_segment(owner) || !crate::store::valid_segment(name) {
        return None;
    }
    let mut suffix = tail.iter().map(|s| escape(s)).collect::<Vec<_>>().join(":");
    let encoded: Vec<String> = tail.iter().map(|s| encode(s)).collect();
    let mut path = format!("/api/{}/{}/{}", encode(owner), encode(name), encoded.join("/"));
    // The query is part of the key, so `log` pagination cannot serve page one for page two. A `#`
    // in it is a FRAGMENT to `Url::parse` and never reaches upstream — the key and the request
    // would diverge again, so it is refused rather than trimmed.
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        if q.contains('#') {
            return None;
        }
        suffix.push('?');
        suffix.push_str(q);
        path.push('?');
        path.push_str(q);
    }
    Some(Parsed { repo: format!("{owner}/{name}"), suffix, path })
}

/// The token a client presented, Basic (git's own shape: `x:<token>`) or Bearer.
fn bearer_or_basic(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.to_string());
    }
    let d = base64::engine::general_purpose::STANDARD
        .decode(v.strip_prefix("Basic ")?)
        .ok()?;
    String::from_utf8(d)
        .ok()?
        .split_once(':')
        .map(|(_, p)| p.to_string())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"rustic-git\"")],
        "auth required",
    )
        .into_response()
}

/// A private repo and a missing repo must be indistinguishable — including in their headers, so
/// this is built exactly as a forwarded 404 is.
fn not_found() -> Response {
    body_response(StatusCode::NOT_FOUND, false, "", "not found".into())
}

/// Nothing downstream may keep a private answer. Public answers keyed by an object id are true
/// forever; public `refs` is only true for as long as the cache holds it.
fn cache_control(public: bool, suffix: &str) -> &'static str {
    match (public, suffix.starts_with("refs")) {
        (false, _) => "private, no-store",
        (true, true) => "public, max-age=5",
        (true, false) => "public, max-age=31536000, immutable",
    }
}

fn body_response(
    status: StatusCode,
    public: bool,
    suffix: &str,
    body: axum::body::Bytes,
) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, cache_control(public, suffix)),
            (header::CONTENT_TYPE, "application/json"),
        ],
        body,
    )
        .into_response()
}

impl Api {
    /// Is this repo public? `None` means "cannot decide here", which sends the request upstream
    /// where the repo database can answer.
    async fn visibility(&self, repo: &str) -> Option<bool> {
        match self.cache.get(repo, META).await.as_deref() {
            Some(b"1") => Some(true),
            Some(b"0") => Some(false),
            _ => None,
        }
    }
}

/// Buffer an upstream reply, refusing anything past `MAX_BODY` instead of holding it in memory.
async fn read_bounded(mut r: reqwest::Response) -> Result<axum::body::Bytes> {
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(crate::err("upstream reply is too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.into())
}

/// Who is browsing, expressed as the owner string the git nodes authorize against
/// (`auth::authorize` compares it to the repo's owner). `None` is anonymous.
///
/// Two kinds of credential reach here and they are not interchangeable:
///
///   * A GIT token — what `git clone` sends. It maps to exactly one owner, which
///     is the identity the fleet has always understood.
///   * A SESSION token — what the web app holds. Its subject is an email, which
///     means nothing to a git node: repos are owned by handles, and a person may
///     act under their own handle or any team they belong to. So the api tier
///     resolves the question it is uniquely able to answer — is this person a
///     member of THIS repo's owner? — and, when they are, presents them upstream
///     as that owner.
///
/// Presenting as the owner is not an escalation: the api already holds the peer
/// secret, which grants a caller the right to be told any private repo's contents.
/// This narrows that blanket trust to the one namespace the caller belongs to.
async fn browse_caller(
    api: &Api,
    headers: &HeaderMap,
    repo_owner: &str,
) -> std::result::Result<Option<String>, Response> {
    let Some(token) = bearer_or_basic(headers) else {
        return Ok(None);
    };
    // A session token first, and only when it verifies: an unverifiable string is
    // not treated as a session, it falls through to the git-token lookup, which is
    // what `git clone` over Basic auth actually sends.
    if let Some(jwt) = api.jwt.as_deref() {
        if let Ok(claims) = jwt.verify(&token) {
            let Some(db) = api.directory.as_deref() else {
                // A session is presented but membership cannot be established, so
                // the only honest answer is "no better than anonymous".
                return Ok(None);
            };
            return match may_act_under(db, &claims.sub, repo_owner).await {
                Ok(true) => Ok(Some(repo_owner.to_string())),
                Ok(false) => Ok(None),
                Err(e) => {
                    eprintln!("browse authorization: {e}"); // ponytail: eprintln
                    Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
                }
            };
        }
    }
    match api.store.owner_for_token(&token).await {
        Ok(Some(o)) => Ok(Some(o)),
        Ok(None) => Err(unauthorized()),
        Err(e) => {
            eprintln!("token lookup: {e}"); // ponytail: eprintln
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let Some(Parsed { repo, suffix, path }) = split_api_path(&path, query.as_deref()) else {
        return not_found();
    };
    let owner_of_repo = repo.split('/').next().unwrap_or_default().to_string();
    let caller = match browse_caller(&api, req.headers(), &owner_of_repo).await {
        Ok(c) => c,
        Err(r) => return r,
    };

    // Serve from cache only when this caller is entitled to it without asking a git node.
    if let Some(public) = api.visibility(&repo).await {
        if !crate::auth::authorize(caller.as_deref(), &owner_of_repo, public) {
            return if caller.is_none() {
                unauthorized()
            } else {
                not_found()
            };
        }
        if let Some(body) = api.cache.get(&repo, &suffix).await {
            return body_response(StatusCode::OK, public, &suffix, body.into());
        }
    }

    // Read BEFORE the upstream call, and written back under THIS value: a purge landing while the
    // request is in flight bumps the generation, so the write below lands in a generation nothing
    // can reach rather than in the freshly emptied one. Only on the miss path.
    // A Redis error or timeout makes `generation` answer 1 rather than fail; on a purged repo that
    // is not the current generation, so the write is unreachable — the safe direction.
    let generation = api.cache.generation(&repo).await;
    // Rebuilt from the parsed segments, never from `req.uri()`: reqwest's URL parsing removes dot
    // segments, so a raw path could authorize as one repo and be served as another.
    let url = format!("{}{}", api.upstream, path);
    let mut up = api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret);
    if let Some(c) = &caller {
        up = up.header(crate::proxy::OWNER_HEADER, c);
    }
    let r = match up.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = match read_bounded(r).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("upstream body: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    // An anonymous caller that upstream served is proof the repo is public; a rejected one proves
    // nothing (private and missing look alike, deliberately). An authenticated caller proves
    // nothing either way, so only the anonymous success writes the flag.
    let public = caller.is_none() && status.is_success();
    if public {
        api.cache.put_at(generation, &repo, META, b"1", TTL_META).await;
    }
    // Only public bodies. An owner-authenticated read of a private repo is a success too, but a
    // read can only reach a cached body through `META`, which only an anonymous success writes —
    // so the entry would be unreachable by construction, buying nothing and risking everything.
    if public && body.len() <= MAX_CACHED_BODY {
        let ttl = if suffix.starts_with("refs") {
            TTL_REFS
        } else {
            TTL_IMMUTABLE
        };
        api.cache.put_at(generation, &repo, &suffix, &body, ttl).await;
    }
    body_response(status, public, &suffix, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str, query: Option<&str>) -> Option<(String, String, String)> {
        split_api_path(path, query).map(|p| (p.repo, p.suffix, p.path))
    }

    /// Catches: an unvalidated repo name, where `alice/web:c/tree/x` and `alice/web` + `c/tree/x`
    /// produce the same cache key.
    #[test]
    fn a_repo_name_that_is_not_a_repo_name_is_refused() {
        assert!(p("/api/alice/web:c/tree/x", None).is_none());
        assert!(p("/api/al ice/web/tree/x", None).is_none());
        assert!(p("/api/api/web/tree/x", None).is_some()); // owner reserved at create, not here
        assert!(p("/api/alice/web/tree/x", None).is_some());
    }

    #[test]
    fn a_browse_path_becomes_a_repo_a_key_and_the_path_to_forward() {
        assert_eq!(
            p("/api/alice/web/tree/abc/src", None),
            Some((
                "alice/web".into(),
                "tree:abc:src".into(),
                "/api/alice/web/tree/abc/src".into()
            ))
        );
        // Pagination has to vary both the key and the forwarded path, or page two serves page one.
        assert_eq!(
            p("/api/alice/web/log/abc", Some("page=2")),
            Some((
                "alice/web".into(),
                "log:abc?page=2".into(),
                "/api/alice/web/log/abc?page=2".into()
            ))
        );
        // Not browse routes: no tail, no name, not under /api/.
        assert_eq!(p("/api/alice/web", None), None);
        assert_eq!(p("/api/alice", None), None);
        assert_eq!(p("/alice/web.git/info/refs", None), None);
    }

    #[test]
    fn a_dot_segment_is_refused_in_every_spelling() {
        // `url::Url::parse`, inside reqwest, strips all of these — so a guard that compares raw
        // text against `".."` alone lets the encoded spellings through, authorizing alice/web and
        // fetching bob/private.
        for seg in [
            "..", "%2e%2e", "%2E%2E", "%2e.", ".%2E", ".", "%2e", "%2E", "", "%2f", "%5C",
        ] {
            assert_eq!(
                p(&format!("/api/alice/web/tree/{seg}/abc"), None),
                None,
                "segment {seg:?} must be refused"
            );
        }
        assert_eq!(p("/api/%2e%2e/bob/private/refs", None), None);
        // A `#` in the query is a fragment to `Url::parse`: it would key one thing and request
        // another.
        assert_eq!(p("/api/alice/web/log/abc", Some("page=2#x")), None);
        // A malformed escape is refused rather than guessed at.
        assert_eq!(p("/api/alice/web/tree/%zz", None), None);
    }

    #[test]
    fn the_forwarded_path_is_the_path_that_was_validated() {
        // Re-encoded from the DECODED segment, so nothing reqwest strips can survive the rebuild.
        let (_, _, path) = p("/api/alice/web/tree/a%20b", None).unwrap();
        assert_eq!(path, "/api/alice/web/tree/a%20b");
        let (_, _, path) = p("/api/alice/web/tree/a:b", None).unwrap();
        assert_eq!(path, "/api/alice/web/tree/a%3Ab");
    }

    #[test]
    fn distinct_paths_never_share_a_cache_entry() {
        // `:` is the suffix separator, so a `:` inside a segment has to be escaped or two
        // different upstream paths answer from one entry.
        let two_segments = p("/api/alice/web/tree/a/b", None).unwrap();
        let one_colon = p("/api/alice/web/tree/a:b", None).unwrap();
        assert_ne!(two_segments.1, one_colon.1);
        // Two spellings of the SAME segment are one request, so they share a key and a forwarded
        // path — the alias that matters is two different requests colliding, not this.
        let encoded_colon = p("/api/alice/web/tree/a%3Ab", None).unwrap();
        assert_eq!(one_colon, encoded_colon);
        // `?` separates the query in the suffix grammar, and a decoded segment can now contain
        // one: without escaping it, these two distinct requests share an entry and poison each
        // other's answer.
        assert_ne!(
            p("/api/alice/web/tree/a%3Fpage=2", None).unwrap().1,
            p("/api/alice/web/tree/a", Some("page=2")).unwrap().1
        );
    }

    #[test]
    fn a_private_answer_is_never_cacheable_downstream() {
        assert_eq!(cache_control(false, "tree:abc"), "private, no-store");
        assert_eq!(cache_control(false, "refs"), "private, no-store");
        assert_eq!(cache_control(true, "refs"), "public, max-age=5");
        assert_eq!(
            cache_control(true, "tree:abc"),
            "public, max-age=31536000, immutable"
        );
    }
}

// ── teams ───────────────────────────────────────────────────────────────────
//
// Callers are trusted infrastructure, not browsers: the web app holds the peer
// secret and states which signed-in user it is acting for. The end user's
// identity is never taken from anything the browser can set.

#[derive(serde::Deserialize)]
struct NewTeam {
    slug: String,
    name: String,
}

/// Who is asking.
///
/// A signed token first: it proves the identity by itself, so no trust in the
/// caller is required. The peer secret plus an asserted identity is the fallback
/// for service-to-service calls that have no user token yet — notably sign-in,
/// which is where a token comes FROM.
fn caller(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let jwt = api
            .jwt
            .as_deref()
            .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
        return match jwt.verify(bearer.trim()) {
            Ok(c) => Ok(c.sub),
            // Never say which of signature, algorithm or expiry failed.
            Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()),
        };
    }
    let peer = headers
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // Constant-time: a byte-by-byte compare on a shared secret leaks its prefix.
    if peer.len() != api.secret.len()
        || peer
            .bytes()
            .zip(api.secret.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
    match headers.get(crate::proxy::OWNER_HEADER).and_then(|v| v.to_str().ok()) {
        Some(u) if !u.trim().is_empty() => Ok(u.trim().to_string()),
        _ => Err((StatusCode::BAD_REQUEST, "caller identity required").into_response()),
    }
}

fn directory(api: &Api) -> std::result::Result<&crate::directory::Directory, Response> {
    api.directory
        .as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "teams database not configured").into_response())
}

async fn create_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewTeam>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match db.create(body.slug.trim(), &body.name, &user).await {
        Ok(Some(team)) => (StatusCode::CREATED, axum::Json(team)).into_response(),
        // Taken, not an error: the caller shows "that handle is in use" and the
        // form stays on screen.
        Ok(None) => (StatusCode::CONFLICT, "handle already taken").into_response(),
        Err(e) => {
            let msg = e.to_string();
            // A rejected handle is the caller's mistake; anything else is ours and
            // must not echo the database's words back to a user.
            if msg.contains("invalid team handle") || msg.contains("team name required") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("create team: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not create team").into_response()
        }
    }
}

async fn list_teams(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match db.for_user(&user).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list teams: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list teams").into_response()
        }
    }
}

/// What sign-in answers with: who they are, and the token to present next time.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SignIn {
    user: crate::directory::User,
    /// `None` when the server has no signing key: the user still exists, but the
    /// caller must keep using the peer path rather than silently treating an
    /// absent token as a valid one.
    token: Option<String>,
    expires_in: u64,
}

#[derive(serde::Deserialize)]
struct NewUser {
    email: String,
    #[serde(default)]
    name: String,
}

async fn upsert_user(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewUser>,
) -> Response {
    // The caller header is the peer's assertion of who signed in; the body must
    // agree with it. Taking the email from the body alone would let a caller that
    // holds the peer secret mint any identity it likes.
    let asserted = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if asserted.to_lowercase() != body.email.trim().to_lowercase() {
        return (StatusCode::BAD_REQUEST, "caller identity does not match the body").into_response();
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.upsert_user(&body.email, &body.name).await {
        Ok(u) => {
            // The token is minted here and nowhere else, so the signing key lives
            // in one process. The web app receives it and presents it on every
            // later call rather than re-asserting who the user is.
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint(&u.email, &u.name, u.username.as_deref()) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!("minting token: {e}"); // ponytail: eprintln
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: crate::jwt::TTL_SECS }).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("valid email") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("upsert user: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not record user").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct NewUsername {
    username: String,
}

async fn claim_username(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewUsername>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.claim_username(&user, &body.username).await {
        Ok(Some(u)) => {
            // A new token: the old one says they have no handle, and every caller
            // reads that claim rather than asking again.
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint(&u.email, &u.name, u.username.as_deref()) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!("minting token: {e}"); // ponytail: eprintln
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: crate::jwt::TTL_SECS }).into_response()
        }
        Ok(None) => (StatusCode::CONFLICT, "that handle is taken").into_response(),
        Err(e) => {
            let msg = e.to_string();
            // Every rule in check_handle is the caller's to fix, and the message
            // says which rule — it is shown under the field.
            if msg.contains("handle") || msg.contains("username already set") || msg.contains("no such user") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("claim username: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not claim that handle").into_response()
        }
    }
}

// ── repos ───────────────────────────────────────────────────────────────────

/// A repo as the web sees it.
///
/// The stored `createdAt` is a BSON date, which serde renders as
/// `{"$date":{"$numberLong":"…"}}` — an encoding a browser has no business
/// parsing. The wire shape is milliseconds, which `new Date(n)` reads directly.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoOut {
    #[serde(rename = "_id")]
    id: String,
    owner: String,
    name: String,
    public: bool,
    description: String,
    created_by: String,
    created_at: i64,
}

impl From<crate::directory::Repo> for RepoOut {
    fn from(r: crate::directory::Repo) -> Self {
        RepoOut {
            id: r.id,
            owner: r.owner,
            name: r.name,
            public: r.public,
            description: r.description,
            created_by: r.created_by,
            created_at: r.created_at.timestamp_millis(),
        }
    }
}

#[derive(serde::Deserialize)]
struct NewRepo {
    /// The namespace: the caller's own handle, or a team they belong to.
    owner: String,
    name: String,
    /// Absent means private, matching the node route it forwards to.
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    description: String,
}

/// May `user` (an email) create under `owner`?
///
/// Two ways to qualify and no third: it is their own handle, or they are a member
/// of the team of that name. Roles are not distinguished — a member who cannot
/// create a repo is a member who cannot do the work — but membership is required,
/// so holding a session is never on its own enough to write into a namespace.
///
/// A team that does not exist and a team the caller is not in give the same
/// answer, so this cannot be used to enumerate teams.
async fn may_act_under(
    db: &crate::directory::Directory,
    user: &str,
    owner: &str,
) -> Result<bool> {
    if let Some(u) = db.user(user).await? {
        if u.username.as_deref() == Some(owner) {
            return Ok(true);
        }
    }
    Ok(db
        .get(owner)
        .await?
        .is_some_and(|t| t.members.iter().any(|m| m.user.eq_ignore_ascii_case(user))))
}

async fn create_repo(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewRepo>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (owner, name) = (body.owner.trim(), body.name.trim());
    let visibility = match body.visibility.as_deref() {
        None | Some("private") => "private",
        Some("public") => "public",
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    // Validated HERE as well as on the node: this builds a URL from these two
    // strings, and a name carrying a slash or a dot segment would address a
    // different route than the one authorized just above.
    if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
        return (StatusCode::BAD_REQUEST, "invalid repository name").into_response();
    }
    // After the request has been judged on its own terms: a malformed name is
    // refused the same way whether or not the database happens to be reachable.
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        // Not 403: whether a team exists is not this caller's business to learn.
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not create repository").into_response();
        }
    }

    // The name is claimed in the index BEFORE the fleet is asked to create it, so
    // uniqueness is one atomic insert rather than a check-then-insert two requests
    // could interleave. Everything after this unwinds the claim on the way out.
    let repo = match db.claim_repo(owner, name, visibility == "public", &body.description, &user).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::CONFLICT, "a repository of that name already exists").into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("invalid repository name") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("claim repo: {msg}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not create repository").into_response();
        }
    };

    let url = format!(
        "{}/api/{}/{}/create?visibility={visibility}",
        api.upstream,
        encode(owner),
        encode(name)
    );
    let sent = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .send()
        .await;
    let status = match &sent {
        Ok(r) => r.status().as_u16(),
        Err(e) => {
            eprintln!("create repo upstream: {e}"); // ponytail: eprintln
            0
        }
    };
    match status {
        201 | 204 => (StatusCode::CREATED, axum::Json(RepoOut::from(repo))).into_response(),
        other => {
            // The repo does not exist on the fleet, so the claim must not outlive
            // this request — otherwise the name is held by nothing and the person
            // who tried to create it cannot try again.
            if let Err(e) = db.forget_repo(owner, name).await {
                eprintln!("unwinding claim {owner}/{name}: {e}"); // ponytail: eprintln
            }
            // 409 here means the fleet holds a repo the index did not know about —
            // an inconsistency, not the caller's mistake, so it is not reported as
            // one. The claim has just been released, so a retry is honest.
            if other != 0 {
                eprintln!("create repo upstream: {other}"); // ponytail: eprintln
            }
            (StatusCode::BAD_GATEWAY, "could not create repository").into_response()
        }
    }
}

/// `GET /v1/repos?owner=X`. Members only: a stranger's view of a namespace is a
/// different question (which repos are PUBLIC), and answering it from here would
/// mean this route decided visibility for two audiences at once.
async fn list_repos(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(owner) = q.get("owner").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "owner is required").into_response();
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not list repositories").into_response();
        }
    }
    match db.repos_for(owner).await {
        Ok(list) => axum::Json(list.into_iter().map(RepoOut::from).collect::<Vec<_>>()).into_response(),
        Err(e) => {
            eprintln!("list repos: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list repositories").into_response()
        }
    }
}

// ── credentials ─────────────────────────────────────────────────────────────
//
// A credential acts in exactly ONE namespace, chosen when it is made, because
// that is what the git fleet enforces: `auth::authorize` compares the credential's
// owner to the repo's owner, with no membership lookup — the nodes have no
// directory. Scoping here to a namespace the caller belongs to keeps the two ends
// saying the same thing, and means a leaked laptop key cannot reach a team's repos
// unless it was made for them.

use crate::directory::{Credential, CredentialKind};

#[derive(serde::Deserialize)]
struct NewCredential {
    owner: String,
    #[serde(default)]
    name: String,
    /// ssh keys only: the OpenSSH public key line.
    #[serde(default)]
    key: String,
}

/// A token, the one time it is readable. Everything else about it can be looked up
/// forever; the secret cannot, because only its digest is kept.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct IssuedToken {
    token: String,
    #[serde(flatten)]
    meta: Credential,
}

/// The caller, and their right to act in `owner`. Every credential route starts
/// here, so none of them can be reached for a namespace that is not the caller's.
async fn credential_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
) -> std::result::Result<(String, &'a crate::directory::Directory), Response> {
    let user = caller(api, headers)?;
    let db = directory(api)?;
    match may_act_under(db, &user, owner).await {
        Ok(true) => Ok((user, db)),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such owner").into_response()),
        Err(e) => {
            eprintln!("credential authorization: {e}"); // ponytail: eprintln
            Err((StatusCode::BAD_GATEWAY, "could not read credentials").into_response())
        }
    }
}

/// `?owner=` for the list routes.
fn owner_param(q: &std::collections::HashMap<String, String>) -> std::result::Result<String, Response> {
    q.get("owner")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "owner is required").into_response())
}

async fn create_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "give the token a name").into_response();
    }
    if name.chars().count() > 60 {
        return (StatusCode::BAD_REQUEST, "that name is too long").into_response();
    }

    // The secret is created FIRST and the index second, so a crash between them
    // leaves a working token nobody can see rather than a listed token that does
    // not work. The unwind below closes that window in the ordinary case.
    let token = match api.store.create_token(&owner).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("create token: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    };
    let meta = Credential {
        id: crate::store::Store::token_digest(&token),
        kind: CredentialKind::Token,
        owner: owner.clone(),
        created_by: user,
        name: name.to_string(),
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        // A digest collision is not a thing that happens; treat it as our failure.
        Ok(None) | Err(_) => {
            if let Err(e) = api.store.revoke_token_digest(&meta.id).await {
                eprintln!("unwinding token: {e}"); // ponytail: eprintln
            }
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    }
    // The only time the token is ever readable.
    (StatusCode::CREATED, axum::Json(IssuedToken { token, meta })).into_response()
}

async fn list_tokens(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.credentials_for(&owner, CredentialKind::Token).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list tokens: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list tokens").into_response()
        }
    }
}

/// Revoke by id. The index is deleted LAST: if the object delete fails the
/// credential stays listed and revocable, which is the safe direction — a listed
/// token that still works can be revoked again, an unlisted one that still works
/// cannot be revoked at all.
async fn revoke_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::Token).await
}

async fn remove_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::SshKey).await
}

async fn revoke(
    api: Arc<Api>,
    headers: axum::http::HeaderMap,
    id: String,
    kind: CredentialKind,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let found = match db.credential(&id).await {
        Ok(Some(c)) if c.kind == kind => c,
        // A credential of the wrong kind is reported as missing rather than as a
        // mistake: the id space is shared, and saying "that is an ssh key" tells a
        // caller something about a credential that may not be theirs.
        Ok(_) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            eprintln!("revoke lookup: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    };
    // Authorized against the credential's OWNER, never against holding its id.
    match may_act_under(db, &user, &found.owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            eprintln!("revoke authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    }
    let gone = match kind {
        CredentialKind::Token => api.store.revoke_token_digest(&id).await,
        CredentialKind::SshKey => api.store.remove_ssh_key(&id).await,
    };
    if let Err(e) = gone {
        eprintln!("revoke: {e}"); // ponytail: eprintln
        return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
    }
    if let Err(e) = db.forget_credential(&id).await {
        // The credential no longer works, which is what was asked for. It will
        // linger in the list until the next attempt succeeds.
        eprintln!("forget credential {id}: {e}"); // ponytail: eprintln
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn add_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Parsed before anything is written, so a malformed key is a 400 rather than a
    // row describing a key the fleet never accepted.
    let fingerprint = match crate::store::Store::ssh_fingerprint(&body.key) {
        Ok(f) => f,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    // The comment at the end of the key line, when they did not name it — which is
    // usually `user@machine` and is exactly what they would have typed.
    let name = match body.name.trim() {
        "" => body.key.split_whitespace().nth(2).unwrap_or("ssh key").to_string(),
        n => n.to_string(),
    };

    let meta = Credential {
        id: fingerprint.clone(),
        kind: CredentialKind::SshKey,
        owner: owner.clone(),
        created_by: user,
        name,
        created_at: mongodb::bson::DateTime::now(),
    };
    // Index first here, unlike a token: the id is the key's own fingerprint rather
    // than a fresh secret, so the insert is what makes "already added" detectable.
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        Ok(None) => return (StatusCode::CONFLICT, "that key is already added").into_response(),
        Err(e) => {
            eprintln!("add key: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
    }
    if let Err(e) = api.store.add_ssh_key(&owner, &body.key).await {
        let _ = db.forget_credential(&meta.id).await;
        eprintln!("add key: {e}"); // ponytail: eprintln
        return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
    }
    (StatusCode::CREATED, axum::Json(meta)).into_response()
}

async fn list_keys(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.credentials_for(&owner, CredentialKind::SshKey).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list keys: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list keys").into_response()
        }
    }
}

// ── passkeys ────────────────────────────────────────────────────────────────
//
// WebAuthn is verified by the web app, which holds the relying-party identity and
// the challenge. This tier stores what verification needs — a public key and a
// counter — and answers the one question a sign-in asks before it knows who is
// signing in: whose credential is this?

use crate::directory::Passkey;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewPasskey {
    id: String,
    public_key: String,
    #[serde(default)]
    counter: i64,
    #[serde(default)]
    transports: Vec<String>,
    #[serde(default)]
    name: String,
}

async fn add_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewPasskey>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if body.id.trim().is_empty() || body.public_key.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a credential id and public key are required").into_response();
    }
    let name = match body.name.trim() {
        "" => "Passkey".to_string(),
        n => n.chars().take(60).collect(),
    };
    let key = Passkey {
        id: body.id.trim().to_string(),
        user: user.to_lowercase(),
        public_key: body.public_key.trim().to_string(),
        counter: body.counter,
        transports: body.transports,
        name,
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_passkey(&key).await {
        Ok(Some(())) => (StatusCode::CREATED, axum::Json(key)).into_response(),
        Ok(None) => (StatusCode::CONFLICT, "that passkey is already registered").into_response(),
        Err(e) => {
            eprintln!("add passkey: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not add the passkey").into_response()
        }
    }
}

async fn list_passkeys(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.passkeys_for(&user).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list passkeys: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list passkeys").into_response()
        }
    }
}

async fn remove_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Owned by the caller, or it does not exist as far as they are concerned.
    match db.passkey(&id).await {
        Ok(Some(p)) if p.user.eq_ignore_ascii_case(&user) => {}
        Ok(_) => return (StatusCode::NOT_FOUND, "no such passkey").into_response(),
        Err(e) => {
            eprintln!("passkey lookup: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not remove the passkey").into_response();
        }
    }
    match db.forget_passkey(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("remove passkey: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not remove the passkey").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct PasskeyLookup {
    id: String,
}

/// Whose passkey is this, and what verifies it?
///
/// PEER ONLY, and deliberately not reachable with a session: it is called during
/// sign-in, when there is no session yet. `caller` enforces that — with no Bearer
/// token it requires the peer secret, so the web app is the only thing that can
/// ask. A credential id is high-entropy and known only to the authenticator and
/// this server, but it still maps to an email, so it is not a public lookup.
async fn lookup_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<PasskeyLookup>,
) -> Response {
    if let Err(r) = caller(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.passkey(body.id.trim()).await {
        Ok(Some(p)) => axum::Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such passkey").into_response(),
        Err(e) => {
            eprintln!("passkey lookup: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not look up the passkey").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct PasskeyUsed {
    counter: i64,
}

/// Record the counter after a successful sign-in. Same peer-only reasoning as the
/// lookup: it happens before a session exists.
async fn passkey_used(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<PasskeyUsed>,
) -> Response {
    if let Err(r) = caller(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.advance_passkey(&id, body.counter).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("passkey counter: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not record the sign-in").into_response()
        }
    }
}

// ── repo settings ───────────────────────────────────────────────────────────
//
// Every route here answers the same two questions first: may this caller act in
// this namespace, and does this repo exist in it. The fleet is then asked to make
// the change, because the fleet is what enforces it — the directory's copy of
// visibility is for a badge in a list, and its copy of a protection rule would be
// a rule no push path can read.

/// The caller may act under `owner`, and `owner/name` is a repo there.
async fn settings_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
    name: &str,
) -> std::result::Result<&'a crate::directory::Directory, Response> {
    let user = caller(api, headers)?;
    let db = directory(api)?;
    if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid repository name").into_response());
    }
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::NOT_FOUND, "no such repository").into_response()),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return Err((StatusCode::BAD_GATEWAY, "could not read the repository").into_response());
        }
    }
    Ok(db)
}

/// Ask the node that owns this repo to do something. Every settings change goes
/// through here, so they all present the peer secret the same way.
async fn ask_owner(api: &Api, path: String) -> std::result::Result<u16, Response> {
    let url = format!("{}{path}", api.upstream);
    match api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .send()
        .await
    {
        Ok(r) => Ok(r.status().as_u16()),
        Err(e) => {
            eprintln!("settings upstream: {e}"); // ponytail: eprintln
            Err((StatusCode::BAD_GATEWAY, "the service is unavailable").into_response())
        }
    }
}

#[derive(serde::Deserialize)]
struct RepoUpdate {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
}

async fn update_repo(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<RepoUpdate>,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let public = match body.visibility.as_deref() {
        None => None,
        Some("public") => Some(true),
        Some("private") => Some(false),
        Some(_) => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };

    // The fleet first, and only then the index: the node's flag is what decides
    // who may read the repo, so a failure must leave the two agreeing on the OLD
    // answer rather than showing a public badge on a private repo.
    if let Some(p) = public {
        let vis = if p { "public" } else { "private" };
        let path = format!("/api/{}/{}/visibility?visibility={vis}", encode(&owner), encode(&name));
        match ask_owner(&api, path).await {
            Ok(200..=299) => {}
            Ok(404) => return (StatusCode::NOT_FOUND, "no such repository").into_response(),
            Ok(s) => {
                eprintln!("visibility upstream: {s}"); // ponytail: eprintln
                return (StatusCode::BAD_GATEWAY, "could not change visibility").into_response();
            }
            Err(r) => return r,
        }
    }
    if let Err(e) = db.update_repo(&owner, &name, body.description.as_deref(), public).await {
        eprintln!("update repo: {e}"); // ponytail: eprintln
        return (StatusCode::BAD_GATEWAY, "could not save the change").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Delete the repo, then forget it. That order is deliberate: the objects are the
/// thing worth removing, and an index row for a repo that is already gone is a
/// listing entry the next delete cleans up — where the reverse is a repo nobody
/// can see and everybody can still clone.
async fn delete_repo(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let path = format!("/api/{}/{}/delete", encode(&owner), encode(&name));
    match ask_owner(&api, path).await {
        Ok(200..=299) => {}
        Ok(s) => {
            eprintln!("delete upstream: {s}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not delete the repository").into_response();
        }
        Err(r) => return r,
    }
    if let Err(e) = db.forget_repo(&owner, &name).await {
        eprintln!("forget repo {owner}/{name}: {e}"); // ponytail: eprintln
        return (StatusCode::BAD_GATEWAY, "the repository was deleted but is still listed").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn list_protection(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let url = format!("{}/api/{}/{}/protect", api.upstream, encode(&owner), encode(&name));
    let r = match api.client.get(url).header(crate::proxy::PEER_HEADER, &api.secret).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("protection upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match read_bounded(r).await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => {
            eprintln!("protection body: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct ProtectionChange {
    pattern: String,
    #[serde(default)]
    remove: bool,
    #[serde(default = "yes")]
    no_force: bool,
    #[serde(default = "yes")]
    no_delete: bool,
}

fn yes() -> bool {
    true
}

async fn set_protection(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<ProtectionChange>,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let pattern = body.pattern.trim();
    if pattern.is_empty() {
        return (StatusCode::BAD_REQUEST, "a branch pattern is required").into_response();
    }
    let mut path = format!(
        "/api/{}/{}/protect?pattern={}",
        encode(&owner),
        encode(&name),
        encode(pattern)
    );
    if body.remove {
        path.push_str("&remove=1");
    } else {
        if !body.no_force {
            path.push_str("&no_force=0");
        }
        if !body.no_delete {
            path.push_str("&no_delete=0");
        }
    }
    match ask_owner(&api, path).await {
        Ok(200..=299) => StatusCode::NO_CONTENT.into_response(),
        Ok(400) => (StatusCode::BAD_REQUEST, "that is not a branch pattern").into_response(),
        Ok(404) => (StatusCode::NOT_FOUND, "no such repository").into_response(),
        Ok(s) => {
            eprintln!("protect upstream: {s}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not save the rule").into_response()
        }
        Err(r) => r,
    }
}
