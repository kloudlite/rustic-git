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
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The visibility flag, cached apart from the answers it guards: it is what lets a hit be served
/// without asking a git node who may read this repo.
/// `%` is always escaped in a suffix, so `%00` is a byte sequence no path can produce: the
/// visibility flag can never collide with a cached answer (`/api/o/n/meta` would otherwise key on
/// exactly this).
pub const META: &str = "%00meta";

pub struct Api {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    /// Base URL of the git peer Service, e.g. `http://rustic-git:8081`.
    pub upstream: String,
    pub secret: String,
    pub client: reqwest::Client,
}

pub async fn serve(
    store: Arc<Store>,
    cache: Arc<Cache>,
    upstream: String,
    secret: String,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    let api = Arc::new(Api {
        store,
        cache,
        upstream: upstream.trim_end_matches('/').to_string(),
        secret,
        client: reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .unwrap_or_default(),
    });
    let app = Router::new()
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

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let Some(Parsed { repo, suffix, path }) = split_api_path(&path, query.as_deref()) else {
        return not_found();
    };
    let caller = match bearer_or_basic(req.headers()) {
        Some(t) => match api.store.owner_for_token(&t).await {
            Ok(Some(o)) => Some(o),
            Ok(None) => return unauthorized(),
            Err(e) => {
                eprintln!("token lookup: {e}"); // ponytail: eprintln
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        },
        None => None,
    };

    // Serve from cache only when this caller is entitled to it without asking a git node.
    if let Some(public) = api.visibility(&repo).await {
        let owner = repo.split('/').next().unwrap_or_default();
        if !crate::auth::authorize(caller.as_deref(), owner, public) {
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
    if status.is_success() && body.len() <= MAX_CACHED_BODY {
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
