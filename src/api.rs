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

/// The visibility flag, cached apart from the answers it guards: it is what lets a hit be served
/// without asking a git node who may read this repo.
const META: &str = "meta";

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
        client: reqwest::Client::new(),
    });
    let app = Router::new()
        .fallback(axum::routing::any(handle))
        .with_state(api);
    axum::serve(listener, app).await?;
    Ok(())
}

/// `/api/{owner}/{name}/{tail...}` -> (`owner/name`, `tail:with:colons[?query]`). The query is
/// part of the suffix so `log` pagination varies the key. Anything else is not a browse route.
fn split_api_path(path: &str, query: Option<&str>) -> Option<(String, String)> {
    let rest = path.trim_start_matches('/').strip_prefix("api/")?;
    let mut it = rest.split('/');
    let owner = it.next().filter(|s| !s.is_empty())?;
    let name = it.next().filter(|s| !s.is_empty())?;
    let tail: Vec<&str> = it.filter(|s| !s.is_empty()).collect();
    if tail.is_empty() {
        return None;
    }
    let mut suffix = tail.join(":");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        suffix.push('?');
        suffix.push_str(q);
    }
    Some((format!("{owner}/{name}"), suffix))
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

/// A private repo and a missing repo must be indistinguishable.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
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

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let Some((repo, suffix)) = split_api_path(&path, query.as_deref()) else {
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

    let url = format!("{}{}", api.upstream, req.uri());
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
    let body = match r.bytes().await {
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
        api.cache.put(&repo, META, b"1", TTL_META).await;
    }
    if status.is_success() && body.len() <= MAX_CACHED_BODY {
        let ttl = if suffix.starts_with("refs") {
            TTL_REFS
        } else {
            TTL_IMMUTABLE
        };
        api.cache.put(&repo, &suffix, &body, ttl).await;
    }
    body_response(status, public, &suffix, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browse_path_becomes_a_repo_and_a_key() {
        assert_eq!(
            split_api_path("/api/alice/web/tree/abc/src", None),
            Some(("alice/web".into(), "tree:abc:src".into()))
        );
        // Pagination has to vary the key, or page two serves page one.
        assert_eq!(
            split_api_path("/api/alice/web/log/abc", Some("page=2")),
            Some(("alice/web".into(), "log:abc?page=2".into()))
        );
        // Not browse routes: no tail, no name, not under /api/.
        assert_eq!(split_api_path("/api/alice/web", None), None);
        assert_eq!(split_api_path("/api/alice", None), None);
        assert_eq!(split_api_path("/alice/web.git/info/refs", None), None);
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
