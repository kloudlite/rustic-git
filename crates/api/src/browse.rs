use super::*;

/// One parsed request. Authorization, the cache key and the upstream URL all come from THIS —
/// never from the raw URI. Deriving them from different strings is how `..` in a path authorizes
/// one repo and reads another: `Url::parse` removes dot segments, a hand-rolled split does not.
pub(crate) struct Parsed {
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
pub(crate) fn escape(seg: &str) -> String {
    seg.replace('%', "%25")
        .replace(':', "%3A")
        .replace('?', "%3F")
}

/// Percent-decode one path segment, exactly once. `None` for a malformed escape or non-UTF-8:
/// nothing legitimate here is either, and guessing is how a decoder becomes a second parser.
pub(crate) fn decode(seg: &str) -> Option<String> {
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
pub(crate) fn encode(seg: &str) -> String {
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
pub(crate) fn split_api_path(path: &str, query: Option<&str>) -> Option<Parsed> {
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
    if !kloudlite_storage::store::valid_segment(owner) || !kloudlite_storage::store::valid_segment(name) {
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
pub(crate) fn bearer_or_basic(headers: &HeaderMap) -> Option<String> {
    kloudlite_core::httpx::bearer_token(headers)
        .map(str::to_string)
        .or_else(|| kloudlite_core::httpx::basic_token(headers))
}

pub(crate) use kloudlite_core::httpx::unauthorized;

/// A private repo and a missing repo must be indistinguishable — including in their headers, so
/// this is built exactly as a forwarded 404 is.
pub(crate) fn not_found() -> Response {
    body_response(StatusCode::NOT_FOUND, false, "", "not found".into())
}

/// Nothing downstream may keep a private answer. Public answers keyed by an object id are true
/// forever; public `refs` is only true for as long as the cache holds it.
pub(crate) fn cache_control(public: bool, suffix: &str) -> &'static str {
    match (public, is_immutable_suffix(suffix)) {
        (false, _) => "private, no-store",
        (true, false) => "public, max-age=5",
        (true, true) => "public, max-age=31536000, immutable",
    }
}

/// Only content-addressed answers may be cached immutable. These are exactly the `BROWSE_TAILS`
/// views (`bins/server/src/router/`) that take an oid — `parse_oid` in `bins/server/src/browse_api/` is what makes
/// them content-addressed. Everything else (`compare`, `refs`, `protect`, ...) resolves a branch
/// name and changes on every push; defaulting those to immutable is how a public repo ends up
/// serving a week-old diff.
pub(crate) fn is_immutable_suffix(suffix: &str) -> bool {
    matches!(
        suffix.split(':').next().unwrap_or(""),
        "blob" | "tree" | "commit" | "log" | "files" | "lastmod" | "signature"
    )
}

pub(crate) fn body_response(
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

/// `(email, owner) → may_act_under`, remembered for a minute.
///
/// Every browse read paid two directory round trips — the user row, then the team row — before
/// the Redis cache was even asked, so a cache HIT still cost Cosmos RU and its latency. Sessions
/// browse the same few namespaces over and over; one answer per person per namespace per minute
/// is what that costs now. A member removed from a team keeps reading for at most that minute,
/// which is no longer than a cached body already outlived a visibility change.
///
/// Per replica and in memory: the api has a handful of replicas and the entry is a bool.
/// ponytail: full sweep at 10k entries, no LRU — a replica would need ten thousand distinct
/// (person, namespace) pairs in one minute to notice.
#[derive(Default)]
pub struct Membership(std::sync::Mutex<std::collections::HashMap<(String, String), (bool, std::time::Instant)>>);

const MEMBERSHIP_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const MEMBERSHIP_CAP: usize = 10_000;

impl Membership {
    fn get(&self, user: &str, owner: &str) -> Option<bool> {
        let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.get(&(user.to_string(), owner.to_string()))
            .filter(|(_, at)| at.elapsed() < MEMBERSHIP_TTL)
            .map(|(yes, _)| *yes)
    }

    fn put(&self, user: &str, owner: &str, yes: bool) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MEMBERSHIP_CAP {
            m.retain(|_, (_, at)| at.elapsed() < MEMBERSHIP_TTL);
        }
        m.insert((user.to_string(), owner.to_string()), (yes, std::time::Instant::now()));
    }
}

/// `may_act_under`, through `Membership`. Only the browse path uses this: a write still asks
/// the directory every time, because a minute of stale "yes" on a read is a tolerable window and
/// on a write it is not.
async fn may_read_under(api: &Api, db: &kloudlite_pulls::directory::Directory, user: &str, owner: &str) -> Result<bool> {
    if let Some(yes) = api.membership.get(user, owner) {
        return Ok(yes);
    }
    let yes = may_act_under(db, user, owner).await?;
    api.membership.put(user, owner, yes);
    Ok(yes)
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
pub(crate) async fn browse_caller(
    api: &Api,
    headers: &HeaderMap,
    repo_owner: &str,
) -> std::result::Result<Option<String>, Response> {
    let Some(token) = bearer_or_basic(headers) else {
        // No credential is anonymous; a credential that does not decode is refused. The registry
        // draws the same line for the same header, and a public listing must not blur it.
        if kloudlite_core::httpx::basic_malformed(headers) {
            return Err(unauthorized());
        }
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
            return match may_read_under(api, db, &claims.sub, repo_owner).await {
                Ok(true) => Ok(Some(repo_owner.to_string())),
                Ok(false) => Ok(None),
                Err(e) => {
                    tracing::error!(reason = "authorization", owner = %repo_owner, error = %e, "browse.read.failed");
                    Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
                }
            };
        }
    }
    match api.store.owner_for_token(&token).await {
        // A Basic username that does not name the token's owner did not verify: refuse it rather
        // than fall through to anonymous. git's `x` placeholder carries no name and is allowed.
        Ok(Some(o)) if kloudlite_core::httpx::basic_user_names(headers, &o, true) => Ok(Some(o)),
        Ok(_) => Err(unauthorized()),
        Err(e) => {
            tracing::error!(reason = "token", error = %e, "credential.read.failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}


pub(crate) async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
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
        if !kloudlite_storage::auth::authorize(caller.as_deref(), &owner_of_repo, public) {
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
        .header(kloudlite_core::peer::PEER_HEADER, &api.secret);
    if let Some(c) = &caller {
        up = up.header(kloudlite_core::peer::OWNER_HEADER, c);
    }
    let r = match up.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(repo = %repo, error = %e, "upstream.request.failed");
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = match kloudlite_core::httpx::read_bounded(r).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(repo = %repo, error = %e, "upstream.body.failed");
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
    body_response(status, public, &suffix, body.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a key built from one string and looked up by another, or the TTL filter
    /// dropping a fresh entry.
    #[test]
    fn membership_remembers_per_person_and_owner() {
        let m = Membership::default();
        assert_eq!(m.get("alice@x", "team"), None);
        m.put("alice@x", "team", true);
        m.put("alice@x", "other", false);
        assert_eq!(m.get("alice@x", "team"), Some(true));
        assert_eq!(m.get("alice@x", "other"), Some(false));
        assert_eq!(m.get("bob@x", "team"), None);
    }

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
