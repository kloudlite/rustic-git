//! The read API's own process. It holds no repository state: it authenticates, consults the
//! cache, and on a miss asks the git fleet, whose routing already knows which node owns what.
//!
//! Browse routes live on the git nodes' PEER listener only, so `upstream` is the peer Service and
//! every forwarded request carries the peer secret plus, when the caller is authenticated, the
//! owner header — exactly the identity a forwarding node presents, so upstream authorizes this
//! the way it authorizes a peer.

use crate::cache::Cache;
use crate::events::{self, Kind};
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
    // Refuse to boot rather than serve `caller`'s empty-secret guard as the only defense —
    // an empty secret is a misconfiguration, not a valid deployment.
    if secret.is_empty() {
        return Err(crate::err("api peer secret must not be empty"));
    }
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
        // Owner-scoped, not repo-scoped: two segments, not the three `handle`'s
        // `split_api_path` requires. Registered ahead of the fallback so it is matched
        // before `handle` ever sees it and refuses it as too short.
        .route("/api/{owner}/images", axum::routing::get(images_proxy))
        // Both writes on an image, same shape as `images_proxy`: registered ahead of the GET-only
        // fallback because they are POSTs, and hand-written because the caller check is "is this
        // exact owner", not the read-side `authorize` a repo's visibility flag drives.
        .route(
            "/api/{owner}/{image}/imagetagdelete",
            axum::routing::post(imagetagdelete_proxy),
        )
        .route("/api/{owner}/{image}/imagedelete", axum::routing::post(imagedelete_proxy))
        .route("/api/{owner}/{image}/imagevisibility", axum::routing::post(imagevisibility_proxy))
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
        // Pull requests. The metadata is this tier's; the commits, the diff and
        // the merge itself are the fleet's.
        .route(
            "/v1/repos/{owner}/{name}/pulls",
            axum::routing::get(list_pulls).post(open_pull),
        )
        .route("/v1/repos/{owner}/{name}/pulls/{number}", axum::routing::get(get_pull))
        .route(
            "/v1/repos/{owner}/{name}/pulls/{number}/comments",
            axum::routing::post(comment_on_pull),
        )
        .route("/v1/repos/{owner}/{name}/pulls/{number}/merge", axum::routing::post(merge_pull))
        .route("/v1/repos/{owner}/{name}/commits", axum::routing::post(commit_patch))
        .route("/v1/activity", axum::routing::get(activity))
        .route("/v1/repos/{owner}/{name}/pulls/{number}/close", axum::routing::post(close_pull))
        .route("/v1/repos/{owner}/{name}/compare", axum::routing::get(compare_branches))
        .route(
            "/v1/repos/{owner}/{name}/commits/{sha}/signature",
            axum::routing::get(verify_commit),
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
    match (public, is_immutable_suffix(suffix)) {
        (false, _) => "private, no-store",
        (true, false) => "public, max-age=5",
        (true, true) => "public, max-age=31536000, immutable",
    }
}

/// Only content-addressed answers may be cached immutable. These are exactly the `BROWSE_TAILS`
/// views (`src/http.rs`) that take an oid — `parse_oid` in `src/http/browse_api.rs` is what makes
/// them content-addressed. Everything else (`compare`, `refs`, `protect`, ...) resolves a branch
/// name and changes on every push; defaulting those to immutable is how a public repo ends up
/// serving a week-old diff.
fn is_immutable_suffix(suffix: &str) -> bool {
    matches!(
        suffix.split(':').next().unwrap_or(""),
        "blob" | "tree" | "commit" | "log" | "files" | "lastmod" | "signature"
    )
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

/// `GET /api/{owner}/images` — the Container Images page. Proxied by hand rather than through
/// `handle`: that path only ever names a repo, and this one names no repo at all, so it does not
/// fit `split_api_path`'s three-segment shape. No caching either — unlike a repo's browse routes,
/// there is no single visibility flag to key a cache entry on; the answer is small and per-team.
async fn images_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path(owner): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    if !crate::store::valid_segment(&owner) {
        return not_found();
    }
    let caller = match browse_caller(&api, &headers, &owner).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    // A team's images are never a stranger's business, and there is no "public team" concept —
    // unlike a repo, which can admit an anonymous reader. Only a verified member of `owner` passes.
    let anonymous = caller.is_none();
    let Some(who) = caller.filter(|c| c == &owner) else {
        return if anonymous { unauthorized() } else { not_found() };
    };
    let url = format!("{}/api/{}/images", api.upstream, encode(&owner));
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &who)
        .send()
        .await
    {
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
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// `POST /api/{owner}/{image}/imagetagdelete` — proxied by hand for the same reason
/// `images_proxy` is: it is a write, and the fallback below only ever forwards a GET.
///
/// The body (the tag name) is forwarded verbatim to the node that owns the image's database.
async fn imagetagdelete_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    image_write_proxy(&api, &owner, &image, "imagetagdelete", &headers, Some(body), None).await
}

/// `POST /api/{owner}/{image}/imagedelete` — same shape as `imagetagdelete_proxy`, no body.
async fn imagedelete_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    image_write_proxy(&api, &owner, &image, "imagedelete", &headers, None, None).await
}

/// `POST /api/{owner}/{image}/imagevisibility?visibility=public|private`.
///
/// The visibility value is PARSED here and re-emitted, so only `public` or `private` ever reaches
/// upstream — the node would reject anything else anyway, but a 400 belongs where the caller can
/// read it.
async fn imagevisibility_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let visibility = match q.get("visibility").map(String::as_str) {
        Some("public") => "public",
        Some("private") => "private",
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    image_write_proxy(&api, &owner, &image, "imagevisibility", &headers, None,
        Some(&format!("visibility={visibility}"))).await
}

/// Shared by both image writes: authorize the caller as exactly `owner` (an image, like a team's
/// image list, is never a stranger's business — there is no public-image concept to fall back on),
/// then forward to the upstream node the same way `images_proxy` reads from it.
async fn image_write_proxy(
    api: &Api,
    owner: &str,
    image: &str,
    tail: &str,
    headers: &HeaderMap,
    body: Option<axum::body::Bytes>,
    // Rebuilt from the parsed value, never forwarded raw: the upstream route reads
    // `?visibility=`, and passing a caller-supplied string through unchecked is how a query
    // becomes a second parser. `None` for the tails that take no query.
    query: Option<&str>,
) -> Response {
    if !crate::store::valid_segment(owner) || !crate::store::valid_segment(image) {
        return not_found();
    }
    let caller = match browse_caller(api, headers, owner).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let anonymous = caller.is_none();
    let Some(who) = caller.filter(|c| c == owner) else {
        return if anonymous { unauthorized() } else { not_found() };
    };
    let url = match query {
        Some(q) => format!("{}/api/{}/{}/{tail}?{q}", api.upstream, encode(owner), encode(image)),
        None => format!("{}/api/{}/{}/{tail}", api.upstream, encode(owner), encode(image)),
    };
    let mut up = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &who);
    if let Some(b) = body {
        up = up.body(b);
    }
    let r = match up.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match read_bounded(r).await {
        Ok(body) => (status, body).into_response(),
        Err(e) => {
            eprintln!("upstream body: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "upstream error").into_response()
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
    // A backend error makes `generation` answer `None` rather than a real generation; the write
    // below is then skipped entirely, never keyed under a wrong generation.
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
    // `generation` is `None` on a backend error: skip both writes rather than key them under a
    // guessed generation, or a purged repo's pre-purge entries become reachable again.
    if let Some(generation) = generation {
        if public {
            api.cache.put_at(generation, &repo, META, b"1", TTL_META).await;
        }
        // Only public bodies. An owner-authenticated read of a private repo is a success too, but
        // a read can only reach a cached body through `META`, which only an anonymous success
        // writes — so the entry would be unreachable by construction, buying nothing and risking
        // everything.
        if public && body.len() <= MAX_CACHED_BODY {
            let ttl = if is_immutable_suffix(&suffix) { TTL_IMMUTABLE } else { TTL_REFS };
            api.cache.put_at(generation, &repo, &suffix, &body, ttl).await;
        }
    }
    body_response(status, public, &suffix, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str, query: Option<&str>) -> Option<(String, String, String)> {
        split_api_path(path, query).map(|p| (p.repo, p.suffix, p.path))
    }

    /// Minimal `Api` for tests that only exercise header/secret logic — an in-memory
    /// store and cache so no real infra is needed to build the struct.
    async fn test_api_with_secret(secret: &str) -> Api {
        let os: Arc<dyn slatedb::object_store::ObjectStore> =
            Arc::new(slatedb::object_store::memory::InMemory::new());
        let store = Store::open(os, std::env::temp_dir(), false).await.unwrap();
        Api {
            store: Arc::new(store),
            cache: Arc::new(Cache::memory()),
            directory: None,
            jwt: None,
            upstream: String::new(),
            secret: secret.to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn test_marker(name: &str, public: bool) -> crate::index::Marker {
        crate::index::Marker {
            name: name.into(),
            public,
            created_by: "alice@example.com".into(),
            created_ms: 1_700_000_000_000,
            description: format!("the {name} repo"),
            manifests: 0,
            updated_ms: 0,
        }
    }

    /// The listing answers from markers alone — this suite has no Mongo fixture at all, so a
    /// marker with no row behind it listing correctly IS the cutover being proven.
    #[tokio::test]
    async fn a_repo_listing_reads_markers_not_mongo_rows() {
        let api = test_api_with_secret("s").await;
        crate::index::write(&api.store.os, crate::index::Kind::Repo, "alice", &test_marker("web", true))
            .await
            .unwrap();
        let out = repo_listing(&api, "alice", true).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "alice/web", "the `owner/name` identity the Mongo `_id` carried");
        assert_eq!(out[0].owner, "alice");
        assert_eq!(out[0].name, "web");
        assert!(out[0].public);
        assert_eq!(out[0].description, "the web repo");
        assert_eq!(out[0].created_by, "alice@example.com");
        assert_eq!(out[0].created_at, 1_700_000_000_000);
    }

    /// The leak test: a caller who is not a member gets `include_private = false`, and the
    /// private name must be absent from the SERIALIZED body, not merely from some filtered
    /// struct — the name itself is the thing that must not escape.
    #[tokio::test]
    async fn a_listing_without_private_access_never_names_a_private_repo() {
        let api = test_api_with_secret("s").await;
        for m in [test_marker("web", true), test_marker("skunkworks", false)] {
            crate::index::write(&api.store.os, crate::index::Kind::Repo, "alice", &m).await.unwrap();
        }
        let body = serde_json::to_string(&repo_listing(&api, "alice", false).await.unwrap()).unwrap();
        assert!(body.contains("web"), "the public repo is still listed");
        assert!(!body.contains("skunkworks"), "a private repo's NAME leaked into a public listing");

        let body = serde_json::to_string(&repo_listing(&api, "alice", true).await.unwrap()).unwrap();
        assert!(body.contains("skunkworks"), "a member sees both prefixes");
    }

    /// Both markers present is a crashed flip; it must read as private, in the listing too.
    #[tokio::test]
    async fn a_repo_with_both_markers_lists_as_private() {
        let api = test_api_with_secret("s").await;
        let m = test_marker("web", true);
        crate::index::put_in_place(&api.store.os, crate::index::Kind::Repo, "alice", &m).await.unwrap();
        crate::index::put_in_place(
            &api.store.os,
            crate::index::Kind::Repo,
            "alice",
            &crate::index::Marker { public: false, ..m },
        )
        .await
        .unwrap();
        assert!(repo_listing(&api, "alice", false).await.unwrap().is_empty(), "fail closed");
        let out = repo_listing(&api, "alice", true).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(!out[0].public);
    }

    /// Opening a PR must publish exactly one `PullOpened` carrying `repo` and `number` — the
    /// contract task 2 exists to satisfy. Exercised directly against `publish_pull_event` rather
    /// than through the HTTP handler: the handler needs a live Mongo-backed `Directory`, which
    /// this test suite has no fixture for, but the publish call itself is what's under test.
    /// The feed's `XREVRANGE` read must come back newest-first and capped at the requested
    /// count — the same guarantee `activity()` leans on to build the PR half of the feed
    /// without a full Mongo scan. Exercised against `Cache` + `pull_event` directly (see
    /// `opening_a_pull_publishes_pull_opened` above for why: `activity()` itself needs a
    /// live Mongo-backed `Directory` this suite has no fixture for).
    #[tokio::test]
    async fn xrevrange_feed_events_are_newest_first_capped_at_n() {
        let api = test_api_with_secret("s").await;
        for n in 1..=3 {
            publish_pull_event(
                &api.cache,
                Kind::PullOpened,
                "alice/web",
                n,
                "alice@example.com",
                "fix the thing",
                "main",
                "fix-it",
            )
            .await;
        }
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 2)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                pull_event(e, "web".to_string())
            })
            .collect();
        assert_eq!(rows.len(), 2, "capped at the requested count of 2");
        assert_eq!(rows[0].title, "opened #3 fix the thing", "newest first");
        assert_eq!(rows[1].title, "opened #2 fix the thing");
        assert_eq!(rows[0].detail, "fix-it into main", "matches pulls_across's format exactly");
    }

    /// The two conditions `activity()` treats as "fall back to `pulls_across`": a stream entry
    /// for a repo the caller cannot see (filtered against the caller's `owner/name` scope, the
    /// same shape `activity()` builds from `repos_for`), and a kind the feed does not show at
    /// all. Either one leaves `stream_events` empty, which is exactly the trigger `activity()`
    /// checks.
    #[tokio::test]
    async fn events_outside_the_feeds_scope_are_dropped_not_shown() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullCommented,
            "alice/web",
            1,
            "alice@example.com",
            "",
            "",
            "",
        )
        .await; // not a kind the feed shows
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "bob/other",
            2,
            "bob@example.com",
            "t",
            "main",
            "h",
        )
        .await; // not the caller's repo at all

        let scope: std::collections::HashSet<String> = ["alice/web".to_string()].into_iter().collect();
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 10)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                if !scope.contains(&e.repo) {
                    return None;
                }
                let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
                pull_event(e, name)
            })
            .collect();
        assert!(rows.is_empty(), "activity() must now fall back to pulls_across");
    }

    /// The owner-scoping leak this replaces: a same-named repo under a DIFFERENT owner
    /// (`bob/web` vs `alice/web`) must never pass alice's scope filter just because the basename
    /// matches — that was the bug (filtering on `e.repo`'s last path segment alone). And the
    /// href on a stream-sourced row must carry the owner, matching `pulls_across`'s hrefs
    /// (`/{owner}/{name}/pulls/{n}`), not the bare `/{name}/pulls/{n}` that used to 404.
    #[tokio::test]
    async fn same_named_repo_under_another_owner_is_excluded_and_href_carries_owner() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "bob/web",
            9,
            "bob@example.com",
            "bob's private title",
            "main",
            "bob-branch",
        )
        .await;
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "alice/web",
            9,
            "alice@example.com",
            "alice's title",
            "main",
            "alice-branch",
        )
        .await;

        // alice's feed scope: only her own `owner/name` rows, never bob's same-named repo.
        let scope: std::collections::HashSet<String> = ["alice/web".to_string()].into_iter().collect();
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 10)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                if !scope.contains(&e.repo) {
                    return None;
                }
                let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
                pull_event(e, name)
            })
            .collect();

        assert_eq!(rows.len(), 1, "bob's same-named repo must be excluded");
        assert!(rows[0].title.contains("alice's title"), "must not leak bob's PR title");
        assert_eq!(rows[0].href, "/alice/web/pulls/9", "href must carry the owner, not just the name");
    }

    #[tokio::test]
    async fn opening_a_pull_publishes_pull_opened() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "alice/web",
            7,
            "alice@example.com",
            "t",
            "main",
            "h",
        )
        .await;
        let stream = api.cache.mem_stream_snapshot();
        assert_eq!(stream.len(), 1, "exactly one event, not zero and not a double-publish");
        let fields = &stream[0].1;
        let get = |k: &str| fields.iter().find(|(fk, _)| fk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("kind"), Some("pull_opened"));
        assert_eq!(get("repo"), Some("alice/web"));
        assert_eq!(get("number"), Some("7"));
    }

    #[tokio::test]
    async fn commenting_publishes_pull_commented() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullCommented,
            "alice/web",
            7,
            "alice@example.com",
            "",
            "",
            "",
        )
        .await;
        let stream = api.cache.mem_stream_snapshot();
        assert_eq!(stream.len(), 1);
        assert!(stream[0].1.iter().any(|(k, v)| k == "kind" && v == "pull_commented"));
    }

    #[tokio::test]
    async fn empty_peer_secret_never_authenticates() {
        let api = test_api_with_secret("").await;
        let mut h = axum::http::HeaderMap::new();
        h.insert(crate::proxy::PEER_HEADER, "".parse().unwrap());
        h.insert(crate::proxy::OWNER_HEADER, "alice".parse().unwrap());
        assert!(caller(&api, &h).is_err());
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
    fn only_oid_keyed_tails_are_immutable() {
        // branch-resolving reads change on every push — never immutable
        assert!(!is_immutable_suffix("compare:base=main:head=dev"));
        assert!(!is_immutable_suffix("protect"));
        assert!(!is_immutable_suffix("refs"));
        // an object addressed by oid is content-addressed — safe to pin
        assert!(is_immutable_suffix("blob:3a5f...:README.md"));
        assert!(is_immutable_suffix("tree:9c1e..."));
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
    if !crate::proxy::secret_eq(peer, &api.secret) {
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

/// An owner's repos for listing, from the listing-index markers rather than the Mongo mirror.
///
/// The markers ARE the listing truth now (spec §6): they are plain object-store keys, so this
/// answers on any node without opening a single repo database, and it cannot disagree with a row
/// that a failed write left behind. `_id` is not lost by leaving Mongo — it always was
/// `owner/name`, which the marker's path already carries.
///
/// `include_private` is the whole security surface: `index::list` only withholds private names
/// when it is `false`, so a caller whose membership has NOT been established must never reach
/// here with `true` — the same contract `image_listing` states for images.
///
/// Newest first, as the Mongo `sort(createdAt: -1)` this replaces was, so the page does not
/// reorder itself at the cutover.
async fn repo_listing(api: &Api, owner: &str, include_private: bool) -> Result<Vec<RepoOut>> {
    let markers =
        crate::index::list(&api.store.os, crate::index::Kind::Repo, owner, include_private).await?;
    let mut out: Vec<RepoOut> = markers
        .into_iter()
        .map(|m| RepoOut {
            id: format!("{owner}/{}", m.name),
            owner: owner.to_string(),
            name: m.name,
            public: m.public,
            description: m.description,
            created_by: m.created_by,
            created_at: m.created_ms,
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
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
    if crate::store::reserved_repo_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("`{name}` is a page in this namespace, so a repository cannot be called it"),
        )
            .into_response();
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

    // The description and creator travel as query parameters because this route takes no body:
    // the owning node writes them into the repo's own database, and `created_at_ms` is the index
    // row's own instant so the two records name the same moment.
    let url = format!(
        "{}/api/{}/{}/create?visibility={visibility}&description={}&created_by={}&created_at_ms={}",
        api.upstream,
        encode(owner),
        encode(name),
        encode(&repo.description),
        encode(&repo.created_by),
        repo.created_at.timestamp_millis(),
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
/// GET a browse route from the owning node, for the feed.
///
/// `None` for anything that did not work. One unreachable or empty repo must not
/// empty the whole feed — a glance at what happened is worth having in part.
async fn feed_get(api: &Api, owner: &str, path: String) -> Option<String> {
    let res = api
        .client
        .get(format!("{}{path}", api.upstream))
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.text().await.ok()
}

/// Turns a stream `events::Event` into a feed row, or `None` for kinds the feed does not show
/// (`PullCommented`, `MergeRequested`, `HeadMoved` — noise for a glance-at-it rail). `title`/
/// `detail` are built the same way `pulls_across` builds them, off the `title`/`base`/`head`
/// the publisher carried on the event — so the two sources render identically for a caller who
/// cannot tell which one answered. An event from before that field existed carries them empty
/// (see `events::from_fields`), so this degrades to a plain "opened #7" rather than failing.
fn pull_event(e: events::Event, name: String) -> Option<Event> {
    let (kind, verb, detail) = match e.kind {
        Kind::PullOpened => ("pull_opened", "opened", format!("{} into {}", e.head, e.base)),
        Kind::PullMerged => ("pull_merged", "merged", format!("into {}", e.base)),
        Kind::PullClosed => ("pull_closed", "closed", format!("into {}", e.base)),
        Kind::PullCommented | Kind::MergeRequested | Kind::HeadMoved => return None,
    };
    // `e.repo` is `owner/name`; the route is `[owner]/[repo]/pulls/[number]`, same as
    // `pulls_across`'s href below — the bare `name` alone 404s.
    let repo = e.repo.clone();
    Some(Event {
        kind: kind.into(),
        href: format!("/{repo}/pulls/{}", e.number),
        title: format!("{verb} #{} {}", e.number, e.title).trim_end().to_string(),
        detail,
        repo: name,
        actor: e.actor,
        at: e.at_ms / 1000,
    })
}

/// One thing that happened, as the feed shows it.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    /// `commit` | `pull_opened` | `pull_merged` | `repo_created`
    kind: String,
    repo: String,
    /// Who did it. The empty string when only the system knows.
    actor: String,
    title: String,
    /// The short thing under the title — a sha, a branch, a number.
    detail: String,
    /// Seconds since the epoch. Formatted by the reader, in their locale.
    at: i64,
    /// Where clicking it goes, relative to the site root.
    href: String,
}

/// The rail's worth of feed, and the whole page's.
///
/// Each repo read costs two upstream round trips, so the depth is what the caller
/// is paying for. A rail is a glance at half a dozen repos; the page is willing to
/// walk further, but still not the whole namespace — an archive would need the
/// event log this deliberately does not keep.
const FEED_EVENTS: usize = 10;
const FEED_EVENTS_MAX: usize = 100;
fn feed_depth(events: usize) -> (usize, usize) {
    if events <= FEED_EVENTS { (6, 5) } else { (20, 20) }
}

/// What has happened lately across an owner's repos.
///
/// DERIVED, not recorded. Nothing writes an event log — the feed is assembled
/// from what the directory and git already know, which means it is correct for
/// repos that existed long before it, and there is no second copy of the truth
/// to drift. The cost is that it can only show what those two sources record: a
/// commit, a change opened or merged, a repo created. A deploy or a pipeline run
/// is not in here because nothing in this system knows about one.
async fn activity(
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
    // Clamped, not rejected: a caller asking for a thousand wants as many as we
    // will give, not an error.
    let want = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(FEED_EVENTS)
        .clamp(1, FEED_EVENTS_MAX);
    let (feed_repos, per_repo) = feed_depth(want);
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("feed authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not read the feed").into_response();
        }
    }

    let repos = match db.repos_for(owner).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("feed repos: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not read the feed").into_response();
        }
    };

    let mut events: Vec<Event> = Vec::new();

    for r in &repos {
        events.push(Event {
            kind: "repo_created".into(),
            repo: r.name.clone(),
            actor: r.created_by.clone(),
            title: format!("created {}", r.name),
            detail: if r.public { "public".into() } else { "private".into() },
            at: r.created_at.timestamp_millis() / 1000,
            href: format!("/{}/{}", r.owner, r.name),
        });
    }

    let ids: Vec<String> = repos.iter().map(|r| r.id.clone()).collect();
    // `owner/name`, not the bare name: `e.repo` on a stream event is also `owner/name`, and a
    // same-named repo under a different owner must never match (that was the leak — filtering
    // on the basename let `bob/web`'s events through alice's `alice/web` feed).
    let scope: std::collections::HashSet<String> =
        repos.iter().map(|r| format!("{}/{}", r.owner, r.name)).collect();
    let stream_events: Vec<Event> = api
        .cache
        .xrevrange("events", want.max(FEED_EVENTS_MAX))
        .await
        .iter()
        .filter_map(|(_, fields)| {
            let e = events::from_fields(fields)?;
            // Events are global, one stream for every repo; the feed is per-owner. Only
            // `repos_for` told us which repos this caller may see, so filter to those, on the
            // full `owner/name` — see `scope` above for why the bare name is not enough.
            if !scope.contains(&e.repo) {
                return None;
            }
            let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
            pull_event(e, name)
        })
        .take(want)
        .collect();

    if !stream_events.is_empty() {
        events.extend(stream_events);
    } else if let Ok(pulls) = db.pulls_across(&ids, want as i64).await {
        // Fallback: the stream is a nudge, never the record (see `crate::events`). Empty
        // could mean "nothing happened" or "Redis is down/absent" — either way the feed
        // must not go blank, so it degrades to the pre-stream Mongo scan.
        for p in pulls {
            let name = p.repo.split('/').next_back().unwrap_or(&p.repo).to_string();
            let href = format!("/{}/pulls/{}", p.repo, p.number);
            // Merged and opened are two events on one change: the feed is about
            // what happened, and both did.
            if let Some(merged) = p.merged_at_ms {
                events.push(Event {
                    kind: "pull_merged".into(),
                    repo: name.clone(),
                    actor: p.author.clone(),
                    title: format!("merged #{} {}", p.number, p.title),
                    detail: format!("into {}", p.base),
                    at: merged / 1000,
                    href: href.clone(),
                });
            }
            events.push(Event {
                kind: "pull_opened".into(),
                repo: name,
                actor: p.author,
                title: format!("opened #{} {}", p.number, p.title),
                detail: format!("{} into {}", p.head, p.base),
                at: p.created_at_ms / 1000,
                href,
            });
        }
    }

    // The commits. Newest repos first, and only a few of them: each is a round
    // trip to the node that owns it, and a feed nobody scrolls should not cost
    // one request per repo in the namespace.
    for r in repos.iter().take(feed_repos) {
        // Two calls, not one: `log` starts from an OID, and the tip of a branch
        // is exactly the thing that changes. Asking for the refs first is also
        // what makes an empty repo cost nothing here.
        let Some(refs) = feed_get(&api, &r.owner, format!(
            "/api/{}/{}/refs", encode(&r.owner), encode(&r.name)
        )).await else { continue };
        let Ok(refs) = serde_json::from_str::<Vec<serde_json::Value>>(&refs) else { continue };
        let tip = refs
            .iter()
            .find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")
                && x.get("name").and_then(|v| v.as_str()).is_some_and(|n| n.ends_with("/main") || n.ends_with("/master")))
            .or_else(|| refs.iter().find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")))
            .and_then(|x| x.get("oid").and_then(|v| v.as_str()));
        let Some(tip) = tip else { continue };

        let Some(body) = feed_get(&api, &r.owner, format!(
            "/api/{}/{}/log/{}?n={per_repo}", encode(&r.owner), encode(&r.name), encode(tip)
        )).await else { continue };
        let Ok(commits) = serde_json::from_str::<Vec<serde_json::Value>>(&body) else { continue };
        for c in commits {
            let oid = c.get("oid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let msg = c.get("message").and_then(|v| v.as_str()).unwrap_or_default();
            let title = msg.lines().next().unwrap_or_default().to_string();
            let at = c.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
            if oid.is_empty() {
                continue;
            }
            events.push(Event {
                kind: "commit".into(),
                repo: r.name.clone(),
                actor: c.get("author").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                title,
                detail: oid.chars().take(7).collect(),
                at,
                href: format!("/{}/{}/commit/{}", r.owner, r.name, oid),
            });
        }
    }

    events.sort_by(|a, b| b.at.cmp(&a.at));
    events.truncate(want);
    axum::Json(events).into_response()
}

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
    // `may_act_under` above established membership, so the private names under this owner are
    // this caller's to see — the same order `images` uses before it passes `true` on.
    match repo_listing(&api, owner, true).await {
        Ok(list) => axum::Json(list).into_response(),
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
    /// Register this key for SIGNING rather than for access. The same key may be
    /// added both ways; they are separate entries because they grant separate
    /// things.
    #[serde(default)]
    signing: bool,
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
        material: String::new(),
        fingerprints: Vec::new(),
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
        // A signing key never authenticates anything, so it was never written to
        // the store the fleet reads. Forgetting the row is the whole of it.
        CredentialKind::SigningKey => Ok(()),
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
    // An armoured OpenPGP block is a signing key and nothing else — it cannot
    // authenticate an ssh connection, so it is only accepted for signing.
    let is_gpg = body.key.contains("BEGIN PGP PUBLIC KEY BLOCK");
    if is_gpg && !body.signing {
        return (
            StatusCode::BAD_REQUEST,
            "a GPG key can only be added as a signing key",
        )
            .into_response();
    }

    // Parsed before anything is written, so a malformed key is a 400 rather than a
    // row describing a key the fleet never accepted.
    let (fingerprint, fingerprints) = if is_gpg {
        match crate::gpg::fingerprints_of(&body.key) {
            // The primary key names the credential; every subkey is indexed, so a
            // signature made by one finds its owner without a scan.
            Ok(all) if !all.is_empty() => (all[0].clone(), all),
            _ => return (StatusCode::BAD_REQUEST, "that is not an OpenPGP public key").into_response(),
        }
    } else {
        match crate::store::Store::ssh_fingerprint(&body.key) {
            Ok(f) => (f.clone(), vec![f]),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
    // The comment at the end of the key line, when they did not name it — which is
    // usually `user@machine` and is exactly what they would have typed.
    let name = match body.name.trim() {
        "" if is_gpg => crate::gpg::emails_of(&body.key)
            .ok()
            .and_then(|e| e.first().cloned())
            .unwrap_or_else(|| "GPG key".to_string()),
        "" => body.key.split_whitespace().nth(2).unwrap_or("ssh key").to_string(),
        n => n.to_string(),
    };

    let meta = Credential {
        // Prefixed, so one key registered for both purposes is two rows.
        id: if body.signing { format!("sign:{fingerprint}") } else { fingerprint.clone() },
        kind: if body.signing { CredentialKind::SigningKey } else { CredentialKind::SshKey },
        owner: owner.clone(),
        created_by: user,
        name,
        // Only a GPG key keeps its material: an ssh signature carries its own.
        material: if is_gpg { body.key.clone() } else { String::new() },
        fingerprints,
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
    // Only an ACCESS key goes to the store the git nodes authenticate against. A
    // signing key there would silently grant push rights to anyone who added a key
    // to prove authorship.
    if !body.signing && !is_gpg {
        if let Err(e) = api.store.add_ssh_key(&owner, &body.key).await {
            let _ = db.forget_credential(&meta.id).await;
            eprintln!("add key: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
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
    let kind = match q.get("kind").map(String::as_str) {
        Some("signing") => CredentialKind::SigningKey,
        _ => CredentialKind::SshKey,
    };
    match db.credentials_for(&owner, kind).await {
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
    // Same order, same reason: the repo's own database is the truth this is moving toward, so
    // it is written before the index row that mirrors it.
    if let Some(d) = body.description.as_deref() {
        let path = format!("/api/{}/{}/description?description={}", encode(&owner), encode(&name), encode(d));
        match ask_owner(&api, path).await {
            Ok(200..=299) => {}
            Ok(404) => return (StatusCode::NOT_FOUND, "no such repository").into_response(),
            Ok(s) => {
                eprintln!("description upstream: {s}"); // ponytail: eprintln
                return (StatusCode::BAD_GATEWAY, "could not save the change").into_response();
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

// ── pull requests ───────────────────────────────────────────────────────────
//
// A PR is metadata pointing at two BRANCHES. It stores no commits and no diff:
// those are computed from the refs on every read, so a push to the branch updates
// what the PR contains — which is what review is. Storing a snapshot would mean a
// PR that can disagree with the code it claims to be about.


/// Read something from the node that owns this repo, and pass its answer through.
/// Read a repo-scoped route from the owning node, as `owner`.
///
/// The peer secret is not an identity. It says "a node in this fleet is asking",
/// and the node still applies the same read check it applies to anyone — so it
/// has to be told WHO is reading, or a private repo answers 401 to a caller who
/// is entitled to it. The caller establishes that entitlement before calling
/// this; `owner` is what it asserts upstream.
async fn read_from_owner(api: &Api, owner: &str, path: String) -> Response {
    let url = format!("{}{path}", api.upstream);
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, owner)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match read_bounded(r).await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => {
            eprintln!("upstream body: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct NewPull {
    title: String,
    #[serde(default)]
    body: String,
    base: String,
    head: String,
}

async fn open_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewPull>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db
        .open_pull(
            &format!("{owner}/{name}"),
            &body.title,
            &body.body,
            body.base.trim(),
            body.head.trim(),
            &user,
        )
        .await
    {
        Ok(pr) => {
            // Publish AFTER the Mongo write succeeds, never before — a lost publish costs a
            // consumer one fallback poll, but publishing on a write that then fails would
            // announce a PR that never existed.
            publish_pull_event(
                &api.cache,
                Kind::PullOpened,
                &pr.repo,
                pr.number,
                &user,
                &pr.title,
                &pr.base,
                &pr.head,
            )
            .await;
            (StatusCode::CREATED, axum::Json(pr)).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("title") || msg.contains("different branch") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("open pull: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not open the change").into_response()
        }
    }
}

/// Fills in `at_ms` and hands off to `events::publish`, which is itself fire-and-forget: never
/// call this before the Mongo write it follows, and never propagate its result with `?`.
///
/// `title`/`base`/`head` are the PR context the feed needs to render `detail` (see
/// `pull_event`) without a second Mongo read. Pass `""` for a kind the feed does not show
/// (`PullCommented`, `MergeRequested`) — never worth fetching the PR just to fill them in.
#[allow(clippy::too_many_arguments)]
async fn publish_pull_event(
    cache: &Cache,
    kind: Kind,
    repo: &str,
    number: i64,
    actor: &str,
    title: &str,
    base: &str,
    head: &str,
) {
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    events::publish(
        cache,
        &events::Event {
            kind,
            repo: repo.to_string(),
            number,
            actor: actor.to_string(),
            at_ms,
            title: title.to_string(),
            base: base.to_string(),
            head: head.to_string(),
        },
    )
    .await;
}

async fn list_pulls(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.pulls_for(&format!("{owner}/{name}")).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list pulls: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list changes").into_response()
        }
    }
}

async fn get_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.pull(&format!("{owner}/{name}"), number).await {
        Ok(Some(pr)) => axum::Json(pr).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such change").into_response(),
        Err(e) => {
            eprintln!("get pull: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not read the change").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct NewComment {
    body: String,
}

async fn comment_on_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewComment>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let repo = format!("{owner}/{name}");
    match db.comment_on_pull(&repo, number, &user, &body.body).await {
        Ok(()) => {
            publish_pull_event(&api.cache, Kind::PullCommented, &repo, number, &user, "", "", "")
                .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("say something") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("comment: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not post the comment").into_response()
        }
    }
}

/// What a branch would bring to another. Straight through to the owning node —
/// this is a read of git, not of the directory.
async fn compare_branches(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    read_from_owner(
        &api,
        &owner,
        format!(
            "/api/{}/{}/compare?base={}&head={}",
            encode(&owner),
            encode(&name),
            encode(base),
            encode(head)
        ),
    )
    .await
}

/// Ask for the change to be merged.
///
/// Answers 202, not 200: the merge is a JOB. It can be slow — a three-way merge
/// on a large tree is real work — and running it inside this request would hold a
/// connection open on a git node that is also serving clones. The worker picks it
/// up; the PR reports where it got to.
async fn merge_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let strategy = match q.get("strategy").map(String::as_str).unwrap_or("fast-forward") {
        s @ ("fast-forward" | "squash" | "merge" | "rebase") => s,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "strategy must be fast-forward, squash, merge or rebase",
            )
                .into_response()
        }
    };

    let repo = format!("{owner}/{name}");
    match db.request_merge(&repo, number, strategy, &user).await {
        Ok(true) => {
            publish_pull_event(&api.cache, Kind::MergeRequested, &repo, number, &user, "", "", "")
                .await;
            (StatusCode::ACCEPTED, "merging").into_response()
        }
        // Not open, or a merge is already in flight. Asking twice must not queue
        // it twice, and saying so is more use than a second "accepted".
        Ok(false) => (
            StatusCode::CONFLICT,
            "this change is not open, or a merge is already under way",
        )
            .into_response(),
        Err(e) => {
            eprintln!("request merge: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not ask for the merge").into_response()
        }
    }
}

async fn close_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    // Forwarded, not written here: the change lives in the repo's own database, and only the
    // owning node may touch it. That handler publishes the event too, so this tier is left with
    // the one question it alone can answer — may this person close it.
    let path = format!(
        "/api/{}/{}/pulls/{number}/close?by={}",
        encode(&owner),
        encode(&name),
        encode(&user)
    );
    match ask_owner(&api, path).await {
        Ok(200..=299) => StatusCode::NO_CONTENT.into_response(),
        Ok(409) => (StatusCode::CONFLICT, "this change is not open").into_response(),
        Ok(404) => not_found(),
        Ok(s) => {
            eprintln!("close pull: upstream said {s}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not close the change").into_response()
        }
        Err(r) => r,
    }
}

// ── commit signatures ───────────────────────────────────────────────────────

/// What a signature amounts to.
///
/// The three answers are deliberately distinct. "Signed by a key we do not know"
/// is not the same as "signed by a key that is not this author's" — the first is
/// a stranger, the second is a mismatch worth looking at — and neither is
/// "unsigned", which is simply the common case and not a warning.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Verification {
    /// `unsigned` | `verified` | `unverified`
    state: &'static str,
    /// The same vocabulary GitHub uses — `valid`, `unknown_key`, `expired_key`,
    /// `bad_email` and so on — so a client branches on a fixed set rather than on
    /// prose that can be reworded.
    reason_code: &'static str,
    /// Who the key belongs to, when we know them.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
    /// Why it is not verified, in words meant for a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct SignatureOf {
    signature: String,
    payload_base64: String,
    author_email: String,
}

/// The api tier's half of a patch: authorize, name the author, forward.
///
/// The api tier never writes objects itself — the owning node does, because one
/// writer per repo is what makes branch protection and ref updates decidable. So
/// this establishes WHO is committing and hands the patch on; the node's
/// `update_refs` still has the last word on whether the branch may move.
async fn commit_patch(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(mut body): axum::Json<serde_json::Value>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }

    // The author is WHO IS SIGNED IN, never what the request said. A caller that
    // could name its own author could write history as somebody else.
    let name_of = api
        .jwt
        .as_deref()
        .and_then(|j| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .and_then(|t| j.verify(t.trim()).ok())
        })
        .map(|c| c.name)
        .unwrap_or_else(|| user.clone());
    let Some(obj) = body.as_object_mut() else {
        return (StatusCode::BAD_REQUEST, "expected an object").into_response();
    };
    obj.insert("authorName".into(), serde_json::Value::String(name_of));
    obj.insert("authorEmail".into(), serde_json::Value::String(user));

    let url = format!("{}/api/{}/{}/patch", api.upstream, encode(&owner), encode(&name));
    let sent = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &owner)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("commit patch: {e}"); // ponytail: eprintln
                return (StatusCode::BAD_REQUEST, "could not read the patch").into_response();
            }
        })
        .send()
        .await;
    let r = match sent {
        Ok(r) => r,
        Err(e) => {
            eprintln!("commit patch: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not reach the repository").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = r.text().await.unwrap_or_default();
    // The node's own words: "this branch has moved since you started editing", or
    // the protection rule that refused it. Both are written for the person at the
    // editor, so they are passed through rather than replaced.
    if status.is_success() {
        (status, [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response()
    } else {
        (status, text).into_response()
    }
}

async fn verify_commit(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, sha)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };

    let url = format!(
        "{}/api/{}/{}/signature/{}",
        api.upstream,
        encode(&owner),
        encode(&name),
        encode(&sha)
    );
    // The peer secret alone is not an identity: this route reads a repo, so the
    // node applies the same read check it applies to any browse request and needs
    // to be told WHO is reading. `settings_caller` has already established that
    // the caller may act under this owner, which is what is asserted here.
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &owner)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("signature upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return (StatusCode::NOT_FOUND, "no such commit").into_response();
    }
    let body = match read_bounded(r).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("signature body: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let signed: Option<SignatureOf> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("signature parse: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let Some(signed) = signed else {
        return axum::Json(Verification {
            state: "unsigned",
            reason_code: "unsigned",
            signer: None,
            reason: None,
        })
        .into_response();
    };

    axum::Json(verify_signature(db, &signed).await).into_response()
}

/// A GPG signature.
///
/// The lookup runs on the fingerprints the SIGNATURE names, because a commit is
/// normally signed by a subkey while the person is the primary key behind it.
/// `signer_by_any` walks that back.
async fn verify_pgp(
    db: &crate::directory::Directory,
    signed: &SignatureOf,
    payload: &[u8],
) -> Verification {
    let issuers = match crate::gpg::issuers(&signed.signature) {
        Ok(i) => i,
        Err(_) => {
            return Verification {
                state: "unverified",
                reason_code: "unknown_signature_type",
                signer: None,
                reason: Some("the signature could not be read".into()),
            }
        }
    };
    let known = match db.signer_by_any(&issuers).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Verification {
                state: "unverified",
                reason_code: "unknown_key",
                signer: None,
                reason: Some("signed by a key nobody here has registered".into()),
            }
        }
        Err(e) => {
            eprintln!("signer lookup: {e}"); // ponytail: eprintln
            return Verification {
                state: "unverified",
                reason_code: "invalid",
                signer: None,
                reason: Some("the signing key could not be looked up".into()),
            };
        }
    };

    use crate::gpg::Reason;
    let reason = crate::gpg::verify(&known.material, &signed.signature, payload, &signed.author_email);
    let words = match reason {
        Reason::Valid => None,
        Reason::RevokedKey => Some("that key has been revoked".to_string()),
        Reason::ExpiredKey => Some("that key had expired".to_string()),
        Reason::Invalid => Some("the signature does not match the commit".to_string()),
        Reason::UnknownKey => Some("the registered key could not be read".to_string()),
        Reason::UnknownSignatureType => Some("the signature could not be read".to_string()),
        Reason::BadEmail => Some(format!(
            "signed by {}, but the commit says {} wrote it",
            known.created_by, signed.author_email
        )),
    };
    Verification {
        state: if reason == Reason::Valid { "verified" } else { "unverified" },
        reason_code: reason.as_str(),
        signer: Some(known.created_by),
        reason: words,
    }
}

/// Judge one signature.
///
/// Two things have to hold for "verified": the signature is good, AND the key
/// belongs to the person the commit says wrote it. A valid signature by somebody
/// else's key is exactly what a forged authorship line looks like, so it reports
/// as unverified with the reason spelled out.
async fn verify_signature(db: &crate::directory::Directory, signed: &SignatureOf) -> Verification {
    use base64::Engine;

    let unverified = |code: &'static str, reason: &str| Verification {
        state: "unverified",
        reason_code: code,
        signer: None,
        reason: Some(reason.to_string()),
    };

    let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(&signed.payload_base64)
    else {
        return unverified("invalid", "the signed content could not be read");
    };

    if crate::gpg::is_pgp(&signed.signature) {
        return verify_pgp(db, signed, &payload).await;
    }
    let Ok(sig) = signed.signature.parse::<russh::keys::ssh_key::SshSig>() else {
        return unverified("unknown_signature_type", "the signature could not be read");
    };

    let fingerprint = sig
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string();
    // Looked up the same way as a GPG key, so one index serves both kinds.
    let known = match db.signer_by_any(&[fingerprint.to_lowercase()]).await {
        Ok(k) => k,
        Err(e) => {
            eprintln!("signer lookup: {e}"); // ponytail: eprintln
            return unverified("invalid", "the signing key could not be looked up");
        }
    };
    let Some(known) = known else {
        return unverified("unknown_key", "signed by a key nobody here has registered");
    };

    // The cryptography last: an unknown key is not worth verifying against, and
    // this order means a bad signature and an unknown signer are never confused.
    let key = russh::keys::PublicKey::from(sig.public_key().clone());
    // `git` is the namespace git signs commits under; a signature made for
    // anything else is not a commit signature.
    if key.verify("git", &payload, &sig).is_err() {
        return unverified("invalid", "the signature does not match the commit");
    }
    if !known.created_by.eq_ignore_ascii_case(signed.author_email.trim()) {
        return Verification {
            state: "unverified",
            reason_code: "bad_email",
            signer: Some(known.created_by.clone()),
            reason: Some(format!(
                "signed by {}, but the commit says {} wrote it",
                known.created_by, signed.author_email
            )),
        };
    }
    Verification { state: "verified", reason_code: "valid", signer: Some(known.created_by), reason: None }
}
