mod browse_api;

use crate::protocol::{receive, upload};
use crate::store::Repo;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::Engine;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

/// Cap on a single request body (compressed bytes on the wire). Axum enforces this in the
/// extractor, BEFORE the handler runs, so an unauthenticated client cannot make the server
/// buffer more than this. Override with RUSTIC_GIT_MAX_BODY (bytes).
fn max_body() -> usize {
    std::env::var("RUSTIC_GIT_MAX_BODY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024) // 2 GiB
}

/// Cap on the decompressed size of a gzipped request body — bounds the zlib-bomb amplification
/// on top of the wire-size limit. 8x the body cap.
fn max_decompressed() -> u64 {
    (max_body() as u64) * 8
}

/// Liveness/readiness. 503 when the object store has stopped answering.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    if !app.store.healthy() {
        return (StatusCode::SERVICE_UNAVAILABLE, "object store unreachable").into_response();
    }
    (
        StatusCode::OK,
        format!("ok ({} warm)", app.store.pool.warm_count()),
    )
        .into_response()
}

/// The ownership protocol, on the peer listener only.
///
/// Line-based bodies — this project has no `serde_json`, and these messages are two or three
/// fields (the same reason `ownership::Entry` encodes itself that way):
///
/// ```text
/// POST /own/claim    "{repo}\n{node}[\nforce]"  -> "granted\n{node}\n{expires_ms}"
///                                                 | "heldby\n{node}\n{expires_ms}"
/// POST /own/renew    "{node}\n{repo}\n{repo}…"   -> one LOST repo per line (empty = all renewed)
/// POST /own/release  "{repo}\n{node}"           -> "" (the entry is shortened, never deleted)
/// ```
///
/// **A follower answers 421 to all three.** It is not the leader and cannot write the map, and the
/// caller's idea of who the leader is has gone stale. It must not proxy the message on either:
/// leadership is derived from a name, so a caller that reached the wrong node is misconfigured,
/// and quietly relaying would hide that.
async fn own_claim(State(app): State<Arc<App>>, body: String) -> Response {
    // Leadership first: a follower must answer 421 whatever the body looks like, or a malformed
    // request to the wrong node reports the wrong problem.
    if let Some(r) = leader_only(&app) {
        return r;
    }
    // An optional third line `force`: the asker could not reach the current holder. See
    // `ownership::decide_force_claim` for what the leader does differently, and what it costs.
    let mut lines = body.trim_end().split('\n');
    let (Some(repo), Some(node), force, None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return (StatusCode::BAD_REQUEST, "repo\nnode[\nforce]").into_response();
    };
    if repo.is_empty() || node.is_empty() || force.is_some_and(|f| f != "force") {
        return (StatusCode::BAD_REQUEST, "repo\nnode[\nforce]").into_response();
    }
    match app.grant_claim(repo, node, force.is_some()).await {
        Ok(crate::ownership::Grant::Granted(e)) => {
            (StatusCode::OK, format!("granted\n{}\n{}", e.node, e.expires_ms)).into_response()
        }
        Ok(crate::ownership::Grant::HeldBy(e)) => {
            (StatusCode::OK, format!("heldby\n{}\n{}", e.node, e.expires_ms)).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn own_renew(State(app): State<Arc<App>>, body: String) -> Response {
    if let Some(r) = leader_only(&app) {
        return r;
    }
    let mut lines = body.trim_end().split('\n');
    let node = lines.next().unwrap_or_default().to_string();
    // An empty node line would be nobody, and `decide_renew` would report every repo lost — a
    // silent instruction to the asker to close everything it holds. Refuse it instead.
    if node.is_empty() {
        return (StatusCode::BAD_REQUEST, "node\nrepo...").into_response();
    }
    let repos: Vec<String> = lines.filter(|l| !l.is_empty()).map(String::from).collect();
    match app.grant_renew(&node, &repos).await {
        Ok(lost) => (StatusCode::OK, lost.join("\n")).into_response(),
        Err(e) => internal(e),
    }
}

async fn own_release(State(app): State<Arc<App>>, body: String) -> Response {
    if let Some(r) = leader_only(&app) {
        return r;
    }
    let Some((repo, node)) = two_lines(&body) else {
        return (StatusCode::BAD_REQUEST, "repo\nnode").into_response();
    };
    match app.grant_release(repo, node).await {
        Ok(()) => (StatusCode::OK, "").into_response(),
        Err(e) => internal(e),
    }
}

/// A node announcing that it is, or is no longer, on its way out. Body: `{node}\n{1|0}`.
///
/// A node reports only about ITSELF; nothing here lets one node say another is unavailable. That
/// distinction is the whole reason this is a message rather than a health check: a node knows it
/// received SIGTERM, and no other node can know that without guessing.
async fn own_draining(State(app): State<Arc<App>>, body: String) -> Response {
    if let Some(r) = leader_only(&app) {
        return r;
    }
    let Some((node, flag)) = two_lines(&body) else {
        return (StatusCode::BAD_REQUEST, "node\n1|0").into_response();
    };
    match app.ownership.set_draining(node, flag == "1").await {
        Ok(()) => (StatusCode::OK, "").into_response(),
        Err(e) => internal(e),
    }
}

/// Exactly two non-empty lines, `repo` then `node`. Not `split_once`: that puts everything after
/// the first newline into `node`, and a node name carrying a newline writes an ambiguous record
/// into the map (an `Entry` is two newline-separated fields).
fn two_lines(body: &str) -> Option<(&str, &str)> {
    let mut it = body.trim_end().split('\n');
    let (repo, node, rest) = (it.next()?, it.next()?, it.next());
    (rest.is_none() && !repo.is_empty() && !node.is_empty()).then_some((repo, node))
}

/// `Some(421)` if this node is not the leader — "misdirected request", which is exactly what it is.
fn leader_only(app: &App) -> Option<Response> {
    if app.is_leader() {
        return None;
    }
    Some(
        (
            StatusCode::MISDIRECTED_REQUEST,
            "not the leader; ask pod zero",
        )
            .into_response(),
    )
}

/// Identity established by a *peer*. `None` on the public listener, always.
#[derive(Clone)]
pub struct Trusted(pub Option<String>);

/// The final path segment of a git route (`/{owner}/{name}/{tail}`).
const GIT_ROUTE_TAILS: [&str; 3] = ["info", "git-upload-pack", "git-receive-pack"];

/// The third segment of a browse route (`/api/{owner}/{name}/{tail}`). Every entry is repo-scoped
/// and peer-only. `visibility` and `create` are the WRITES among them (both POST), which is why
/// they belong here rather than in a separate list — they must be routed to the owner exactly as
/// the reads are, so the node that serves the repo is the node that writes it.
///
/// A route missing from this list is UNREACHABLE — the middleware refuses it
/// before the router ever sees it — so adding a browse route means adding its
/// tail here. `every_browse_route_is_routable` holds the two together.
const BROWSE_TAILS: [&str; 15] = [
    "refs", "tree", "blob", "log", "commit", "files", "lastmod", "compare", "signature",
    "visibility", "create", "delete", "protect", "merge", "patch",
];

/// Whether the path is under the browse prefix. `api` is a RESERVED owner name
/// (`store::valid_owner`), so an `/api/` path can only ever be a browse route — never the git route
/// of a repo owned by `api`. That is what keeps this middleware's answer identical to the one
/// axum's router gives: matchit prefers the static `api` segment, and so do we.
///
/// Deployed data may still contain a repo owned by `api` from before the reservation. Its git-HTTP
/// routes are gone: `route_inner` REFUSES every `/api/` path that is not a browse route, on both
/// listeners. That refusal is load-bearing, not defence in depth — matchit has no browse route with
/// only three segments, so `/api/{owner}/git-upload-pack` would otherwise fall through to
/// `/{owner}/{name}/git-upload-pack` as owner=`api`, reaching a git handler having never been
/// routed. Such a repo is still reachable over SSH, and `admin fork` moves it to a non-reserved
/// owner. Deliberate: an unreachable legacy repo beats a second writer.
fn api_prefixed(path: &str) -> bool {
    path.trim_start_matches('/') == "api"
        || path.trim_start_matches('/').starts_with("api/")
}

/// Whether this path is a git route (`/{owner}/{name}/{info|git-upload-pack|git-receive-pack}`).
/// Never true under `/api/`, per `api_prefixed`.
fn git_shape(path: &str) -> bool {
    if api_prefixed(path) {
        return false;
    }
    let mut it = path.trim_start_matches('/').split('/');
    let (Some(_), Some(_), Some(tail)) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    GIT_ROUTE_TAILS.contains(&tail)
}

/// `Some((owner, name))` when the path is a browse route.
fn api_route(path: &str) -> Option<(&str, &str)> {
    let mut it = path.trim_start_matches('/').strip_prefix("api/")?.split('/');
    let (owner, name, tail) = (it.next()?, it.next()?, it.next()?);
    BROWSE_TAILS.contains(&tail).then_some((owner, name))
}

fn repo_of(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    if crate::registry::is_v2_path(path) {
        let (owner, name) = crate::registry::image_route(path)?;
        return Some(crate::registry::routing_key(owner, name));
    }
    // `/api/{owner}/{name}/...` names a repo exactly as the git routes do; skipping the `api`
    // segment makes both shapes yield the same repo, so both route to the same node. An `/api/`
    // path resolves through `api_route` and nothing else — see `api_prefixed`.
    if api_prefixed(path) {
        let (owner, name) = api_route(path)?;
        let (owner, name) = crate::protocol::parse_repo_path(&format!("{owner}/{name}"))?;
        return Some(format!("{owner}/{name}"));
    }
    let mut it = path.split('/');
    let (owner, name, rest) = (it.next()?, it.next()?, it.next()?);
    if !GIT_ROUTE_TAILS.contains(&rest) {
        return None;
    }
    let (owner, name) = crate::protocol::parse_repo_path(&format!("{owner}/{name}"))?;
    Some(format!("{owner}/{name}"))
}

/// Whether this path has the shape of a git route (`/{owner}/{name}/{info|git-upload-pack|git-receive-pack}`),
/// regardless of whether the segments parse. `route` uses this to tell "not ours to route" from
/// "ours, but malformed": the latter must be refused, never passed to a handler that would decode
/// it and open a repo this node does not own.
fn is_git_route(path: &str) -> bool {
    // `/api/{owner}/{name}/...` is repo-scoped exactly as the git routes are: it must reach the
    // owner, because only the owner holds the database and the packs.
    git_shape(path) || api_route(path).is_some() || crate::registry::image_route(path).is_some()
}

/// Route before handling. Runs ahead of authentication: the damage is done by *opening* a repo's
/// database on the wrong node, so a misrouted request must never reach the handlers. Applied to
/// both listeners — a node receiving a forwarded request consults its own copy of the map (and the
/// leader, if that copy has nothing), bounded by the hop count.
/// Public listener: `/api/...` is not repo-scoped here, so it is never forwarded.
async fn route_public(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    route_inner(app, req, next, false).await
}

/// Peer listener: the browse API lives here, so `/api/...` routes like any other repo path.
async fn route_peer(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    route_inner(app, req, next, true).await
}

async fn route_inner(
    app: Arc<App>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
    peer: bool,
) -> Response {
    let path = req.uri().path().to_string();
    // The browse API is mounted on the peer router only. Treating `/api/...` as repo-scoped on the
    // public listener would forward a client request to the owner's PEER port with the shared
    // secret, serving the peer-only endpoint publicly — and only when this node is NOT the owner.
    // Answered here with a flat 404 rather than passed on: `/api/{o}/info/refs` would otherwise
    // match the PUBLIC router's git route as owner=`api`, and be served without ever being routed.
    if !peer && api_prefixed(&path) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // An `/api/` path that is not a browse route is not routable, and must not fall through: no
    // browse route matches fewer than four segments, so matchit would hand
    // `/api/{owner}/git-upload-pack` to the GIT handler as owner=`api` name=`{owner}` — reaching a
    // repo's database on a node that never checked whether it owns it. Refuse instead.
    if api_prefixed(&path) && api_route(&path).is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // A `/v2/` path that names no image is either one of the three local endpoints — answered
    // here, on any node — or nothing at all. It must not fall through to `repo_of`'s git branch,
    // where `/v2/alice/info/refs` would otherwise be served as owner=`v2` having never routed.
    if crate::registry::is_v2_path(&path) && crate::registry::image_route(&path).is_none() {
        let tail = path.trim_start_matches('/').trim_start_matches("v2").trim_start_matches('/');
        let tail = tail.split('?').next().unwrap_or("");
        if crate::registry::LOCAL_V2.contains(&tail) {
            return next.run(req).await;
        }
        // oci_err does not exist yet — Task 3 adds it and this becomes a proper OCI error body.
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let repo = match repo_of(&path) {
        Some(r) => r,
        // A git route whose repo does not parse — a percent-encoded or otherwise invalid name.
        // Refuse here. Falling through would let the handler DECODE the path and open a repo
        // this node may not own, bypassing routing entirely; that is the invariant this whole
        // middleware exists to hold.
        None if is_git_route(&path) => {
            return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
        }
        None => return next.run(req).await, // /healthz, /own/*, anything else: served locally
    };
    // Absent means fresh (0): the public listener strips this header, so every client request
    // arrives without it and MUST route. Present-but-unparseable means exhausted: a peer sent
    // garbage, and serving here beats bouncing. Conflating the two — "missing = exhausted" — makes
    // the public listener never route at all, and every node opens every repo it is sent.
    let hops: u32 = match req.headers().get(crate::proxy::HOPS_HEADER) {
        None => 0,
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::proxy::MAX_HOPS),
    };
    let route = app.route(&repo).await;
    // Out of hops: never forward again (that is the bound), but never knowingly open a repo we do
    // not own either — a chain that arrives here disagreeing with our own view, or arrives at an
    // unhealthy node, gets 503 rather than a second writer. Same bound, no wrong opens.
    if hops >= crate::proxy::MAX_HOPS {
        return match route {
            crate::ownership::Route::Local => next.run(req).await,
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "routing disagreement at hop limit; retry",
            )
                .into_response(),
        };
    }
    match route {
        crate::ownership::Route::Local => next.run(req).await,
        crate::ownership::Route::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no node may safely serve this repository right now; retry",
        )
            .into_response(),
        crate::ownership::Route::Peer(peer) => {
            let owner = req
                .extensions()
                .get::<Trusted>()
                .and_then(|t| t.0.clone())
                .unwrap_or_default();
            // Keep enough to rebuild a bodyless request, in case the owner has just left. Only
            // GET qualifies: a request with a body cannot be replayed once it has been streamed,
            // and info/refs — the request that actually fails here — is a GET.
            let replay = (req.method() == axum::http::Method::GET).then(|| {
                (
                    req.method().clone(),
                    req.uri().clone(),
                    req.headers().clone(),
                    // Outer layers put things here that the handlers need — the peer-established
                    // identity among them. A rebuilt request without them is not the same request.
                    req.extensions().clone(),
                )
            });
            match app.forwarder.forward(&peer.addr, &owner, hops, req).await {
                Ok(res) => res,
                Err(e) => {
                    // A forward that failed to CONNECT means the owner is not there. That happens
                    // on every roll: the owner releases its lease at SIGTERM and stops answering,
                    // while this node's copy of the map is up to a poll interval behind, so it
                    // forwards into a node that has already gone. Measured, that was essentially
                    // every remaining failure of a rolling restart.
                    //
                    // Recovery asks the LEADER again rather than concluding anything: the old owner
                    // has released, so the map now names whoever holds it, and one more hop gets
                    // there. If it still names the same node, this answers 502 exactly as before.
                    //
                    // Only a connect failure qualifies. Routing runs before authentication, so a
                    // client that could make a forward fail on purpose — pushing half a body and
                    // aborting — must not be able to move a repo; that produces a different error,
                    // and a request with a body is not replayed at all.
                    // Decide replay-ability and the error class BEFORE touching the throttle: a
                    // tuple pattern evaluates every element, so putting the throttle in a tuple with
                    // `replay` would burn the per-repo window on a failed push — a request that can
                    // never be replayed — and starve a concurrent GET of the ask it needs.
                    let recoverable = replay.filter(|_| crate::proxy::is_connect_error(&e));
                    if let Some((method, uri, headers, exts)) =
                        recoverable.filter(|_| app.may_ask_to_recover(&repo))
                    {
                        let rebuild = || {
                            let mut again = axum::extract::Request::new(axum::body::Body::empty());
                            *again.method_mut() = method.clone();
                            *again.uri_mut() = uri.clone();
                            *again.headers_mut() = headers.clone();
                            *again.extensions_mut() = exts.clone();
                            again
                        };
                        // Ask the LEADER, not this node's copy of the map. The copy is up to a
                        // poll interval stale, which is the only reason a wait was ever needed
                        // here; the leader is the authority and answers now. Its reply is also the
                        // corroboration that a timer used to stand in for: `HeldBy` naming the node
                        // we could not reach is independent evidence that the holder still owns the
                        // lease and is simply not answering, rather than merely being slow.
                        let asked = app.claim_to_recover(&repo).await;
                        match asked {
                            // Granted: the previous holder had already released — every graceful
                            // restart lands here, and it is the common case. Serve it now.
                            Ok(crate::ownership::Grant::Granted(e)) if e.node == app.self_name => {
                                return next.run(rebuild()).await
                            }
                            // Someone else holds it, or the leader handed it to a third node:
                            // honour that rather than fight for it.
                            Ok(crate::ownership::Grant::Granted(e))
                            | Ok(crate::ownership::Grant::HeldBy(e))
                                if e.node != peer.name =>
                            {
                                let addr = (app.addr_of)(&e.node);
                                if let Ok(res) =
                                    app.forwarder.forward(&addr, &owner, hops, rebuild()).await
                                {
                                    return res;
                                }
                            }
                            // Still held by the node we could not reach. The leader's answer
                            // proves the holder still owns the lease; it says nothing about whether
                            // the holder is reachable, so it is not on its own grounds to move the
                            // repo — a single dropped connect would fence a healthy node. Try once
                            // more, immediately: a blip succeeds here, a crashed node does not.
                            // That second failure is the corroboration, bought with a round trip
                            // instead of a timer.
                            Ok(_) => {
                                // Re-resolve rather than reusing the address from the first
                                // attempt: this is a fresh decision, and the node's address is
                                // whatever it is NOW.
                                let addr = (app.addr_of)(&peer.name);
                                match app.forwarder.forward(&addr, &owner, hops, rebuild()).await {
                                    Ok(res) => return res,
                                    Err(again) if !crate::proxy::is_connect_error(&again) => {
                                        eprintln!("forwarding {repo} to {}: {again}", peer.name); // ponytail: eprintln
                                        return (StatusCode::BAD_GATEWAY, "peer error").into_response();
                                    }
                                    // Two connect failures, and the leader says it is still theirs:
                                    // the holder went without releasing. Move the repo. This fences
                                    // the old owner if it is in fact alive — its in-flight push
                                    // fails and the client retries, which is the trade against ten
                                    // seconds of 502s.
                                    Err(_) => {}
                                }
                                match app.force_claim(&repo).await {
                                    // We were granted it (or the leader, asked by itself, handed it
                                    // to whoever is least loaded — which may not be us).
                                    Ok(g) => {
                                        let e = match g {
                                            crate::ownership::Grant::Granted(e)
                                            | crate::ownership::Grant::HeldBy(e) => e,
                                        };
                                        if e.node == app.self_name {
                                            return next.run(rebuild()).await;
                                        }
                                        // Lost the race, or it moved elsewhere: honour the winner.
                                        if e.node != peer.name {
                                            let addr = (app.addr_of)(&e.node);
                                            if let Ok(res) = app
                                                .forwarder
                                                .forward(&addr, &owner, hops, rebuild())
                                                .await
                                            {
                                                return res;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("force-claiming {repo}: {e}"); // ponytail: eprintln
                                    }
                                }
                            }
                            // The leader is unreachable, or refused: answer as before.
                            Err(e) => eprintln!("claim after failed forward to {repo}: {e}"), // ponytail: eprintln
                        }
                    }
                    eprintln!("forwarding {repo} to {}: {e}", peer.name); // ponytail: eprintln
                    (StatusCode::BAD_GATEWAY, "peer error").into_response()
                }
            }
        }
    }
}

/// Peer listener admission: the secret, then the identity the caller established.
async fn trust_peer(
    State(app): State<Arc<App>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = req
        .headers()
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // ponytail: plain compare; the secret is 64 hex chars and the port needs network reach. Use
    // subtle::ConstantTimeEq if this port is ever exposed more widely.
    if presented.is_empty() || presented != app.forwarder.secret {
        return (StatusCode::FORBIDDEN, "peer secret").into_response();
    }
    let owner = req
        .headers()
        .get(crate::proxy::OWNER_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    req.extensions_mut().insert(Trusted(owner));
    next.run(req).await
}

/// Public listener: strip every routing header a client could set. Hops especially — a client
/// that could set it to the maximum would force this node to open a repo it does not own.
async fn trust_nobody(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    for h in [
        crate::proxy::HOPS_HEADER,
        crate::proxy::OWNER_HEADER,
        crate::proxy::PEER_HEADER,
    ] {
        req.headers_mut().remove(h);
    }
    req.extensions_mut().insert(Trusted(None));
    next.run(req).await
}

fn git_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
        .layer(axum::extract::DefaultBodyLimit::max(max_body()))
}

/// Client-facing. Layers run outermost-first, and the LAST `.layer()` call is outermost — so
/// `trust_nobody` (added last) runs first, then `route`, then the handler.
pub fn router(app: Arc<App>) -> Router {
    git_routes()
        .route("/healthz", get(healthz))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_public))
        .layer(axum::middleware::from_fn(trust_nobody))
        .with_state(app)
}

/// Peer-facing. `trust_peer` outermost (secret check first, on everything), then `route`, then
/// handlers. `/healthz` and the `/own/*` protocol are inside the secret check on purpose: a claim
/// without the secret must fail loudly (403), not silently succeed and hide a misconfiguration.
/// The `route` middleware ignores non-git paths, so `/own/*` passes straight through it.
pub fn peer_router(app: Arc<App>) -> Router {
    git_routes()
        .merge(browse_api::browse_routes())
        .route("/healthz", get(healthz))
        .route("/own/claim", post(own_claim))
        .route("/own/renew", post(own_renew))
        .route("/own/release", post(own_release))
        .route("/own/draining", post(own_draining))
        .layer(axum::middleware::from_fn_with_state(app.clone(), route_peer))
        .layer(axum::middleware::from_fn_with_state(app.clone(), trust_peer))
        .with_state(app)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"rustic-git\"")],
        "auth required",
    )
        .into_response()
}

fn internal(e: crate::Error) -> Response {
    eprintln!("internal error: {e}"); // ponytail: eprintln; swap for a logger when one exists
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// A request the client sent us that we will never satisfy, as opposed to something broken on our
/// end. Distinguished from a bare `crate::err` so `info_refs` can answer 400, not 500, without
/// masking a genuine internal failure the same way.
#[derive(Debug)]
struct ClientError(String);
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ClientError {}

fn client_err(msg: impl Into<String>) -> crate::Error {
    ClientError(msg.into()).into()
}

fn bad_request(e: &crate::Error) -> Response {
    (StatusCode::BAD_REQUEST, e.to_string()).into_response()
}

fn fenced_elsewhere() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "repository is owned by another node; retry",
    )
        .into_response()
}

async fn open(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    read_only: bool,
) -> Result<Repo, Response> {
    // A peer already authenticated this client; its word is trusted because `trust_peer` has
    // checked the shared secret. The public listener always presents `Trusted(None)`.
    let auth_owner = match &trusted.0 {
        Some(o) => Some(o.clone()),
        None => {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Basic "))
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .and_then(|d| String::from_utf8(d).ok())
                .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
            match token {
                Some(t) => {
                    let owner = app.store.owner_for_token(&t).await.map_err(internal)?;
                    if owner.is_none() {
                        return Err(unauthorized());
                    }
                    owner
                }
                // No credentials is not yet a failure: a public repo may still admit this caller.
                None => None,
            }
        }
    };
    // Parsed before the visibility check: the raw path segment still carries `.git`, and looking
    // that up would warm a second, bogus pool entry alongside the repo's real one.
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return Err((StatusCode::BAD_REQUEST, "invalid repository path").into_response());
    };
    // Gated on `repo_exists`, which asks the object store rather than the pool: `is_public` goes
    // through `db_for`, and that CREATES a database for whatever name it is handed. Unguarded, an
    // anonymous request on the public listener could conjure and warm a repo per mistyped path.
    let public = app.store.repo_exists(&owner, &name).await.unwrap_or(false)
        && app.store.is_public(&owner, &name).await.unwrap_or(false);
    if !crate::auth::authorize(auth_owner.as_deref(), &owner, public && read_only) {
        // No credentials at all gets 401, not 404/403: it tells the client to present a token,
        // whereas a private repo denied to an authenticated stranger looks like FORBIDDEN.
        return Err(if auth_owner.is_none() {
            unauthorized()
        } else {
            StatusCode::FORBIDDEN.into_response()
        });
    }
    match app.store.open_repo(&owner, &name).await {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
        Err(e) if crate::pool::is_fenced(&e) => {
            // Fenced at open time. Routing decides: still ours → evict (on_fenced does) and open
            // once more; not ours → 503 so the client retries against the owner.
            if app.on_fenced(&owner, &name).await {
                match app.store.open_repo(&owner, &name).await {
                    Ok(Some(repo)) => Ok(repo),
                    Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
                    Err(e) => {
                        eprintln!("reopen after fence {owner}/{name}: {e}"); // ponytail: eprintln
                        Err(internal(e))
                    }
                }
            } else {
                Err(fenced_elsewhere())
            }
        }
        Err(e) => {
            eprintln!("open_repo {owner}/{name}: {e}"); // ponytail: eprintln; swap for a logger when one exists
            // We were routed here, so the map names us — and we have just proved we cannot serve.
            // Holding the lease anyway leaves the repo with an owner that cannot open it until the
            // TTL lapses; a forced claim makes that worse, because it fenced a peer to get here.
            // Give the lease back now, so the next request claims fresh instead of waiting.
            // Best-effort: a release that fails only means the TTL does the same job later.
            let repo = format!("{owner}/{name}");
            if let Err(e) = app.release(&repo).await {
                eprintln!("releasing {repo} after a failed open: {e}"); // ponytail: eprintln
            }
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}

/// Signals the (blocking) protocol worker that the client is gone. Axum drops the handler future
/// when the connection closes, so dropping this guard is our disconnect notification — without it
/// an abandoned clone would keep building its pack to completion on a blocking thread.
struct Disconnect(Arc<std::sync::atomic::AtomicBool>);
impl Drop for Disconnect {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn body_reader(headers: &HeaderMap, body: Bytes) -> Box<dyn Read + Send> {
    if headers
        .get(header::CONTENT_ENCODING)
        .map(|v| v == "gzip")
        .unwrap_or(false)
    {
        Box::new(flate2::read::GzDecoder::new(Cursor::new(body)).take(max_decompressed()))
    } else {
        Box::new(Cursor::new(body))
    }
}

/// The repo to run the second attempt against, after a fence that routing says we can still own.
/// `None` means the caller should answer 503.
async fn reopen_after_fence(app: &App, owner: &str, name: &str) -> Option<Repo> {
    if !app.on_fenced(owner, name).await {
        return None;
    }
    app.store.open_repo(owner, name).await.ok().flatten()
}

async fn info_refs(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
) -> Response {
    let service = q.get("service").cloned().unwrap_or_default();
    let repo = match open(&app, &trusted, &headers, &owner, &name, service == "git-upload-pack").await {
        Ok(r) => r,
        Err(r) => return r,
    };
    // NOT the raw Path `owner`/`name`: those still carry the `.git` suffix (every real URL has
    // it), which would name a database that does not exist.
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("version=2"))
        .unwrap_or(false);
    let store = app.store.clone();
    let svc = service.clone();
    let run_protocol = move |repo: Repo| {
        let (store, svc) = (store.clone(), svc.clone());
        async move {
            tokio::task::spawn_blocking(move || -> crate::Result<Vec<u8>> {
                let mut out = Vec::new();
                match svc.as_str() {
                    "git-upload-pack" => {
                        if !v2 {
                            return Err(client_err(
                                "this server requires git protocol v2 for git-upload-pack.\n\
                                 git 2.26+ uses protocol v2 by default; older clients can opt in \
                                 with:\n\n  git -c protocol.version=2 <command>\n",
                            ));
                        }
                        upload::advertise(&mut out)?;
                    }
                    "git-receive-pack" => {
                        crate::pktline::write_text(&mut out, "# service=git-receive-pack")?;
                        crate::pktline::write_flush(&mut out)?;
                        receive::advertise(&store, &repo, &mut out)?;
                    }
                    _ => return Err(client_err(format!("unknown service: {svc}"))),
                }
                Ok(out)
            })
            .await
        }
    };
    let success = |out: Vec<u8>| {
        (
            [
                (
                    header::CONTENT_TYPE,
                    format!("application/x-{service}-advertisement"),
                ),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
            out,
        )
            .into_response()
    };
    let res = match run_protocol(repo).await {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    match res {
        Ok(out) => success(out),
        // See App::on_fenced. If routing still says we own it, reopen and run the request again.
        Err(e) if crate::pool::is_fenced(&e) => match reopen_after_fence(&app, &o, &n).await {
            None => fenced_elsewhere(),
            Some(repo) => match run_protocol(repo).await {
                Ok(Ok(out)) => success(out),
                // a second fence is a real error, not retried again
                Ok(Err(e)) if e.downcast_ref::<ClientError>().is_some() => bad_request(&e),
                Ok(Err(e)) => internal(e),
                Err(e) => internal(crate::err(e.to_string())),
            },
        },
        Err(e) if e.downcast_ref::<ClientError>().is_some() => bad_request(&e),
        Err(e) => internal(e),
    }
}

// ponytail: whole request/response buffered in memory; stream when repos get big
async fn upload_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let store = app.store.clone();
    let hs = headers.clone();
    let run_protocol = move |repo: Repo, body: Bytes| {
        let (store, flag, hs) = (store.clone(), flag.clone(), hs.clone());
        async move {
            let mut input = std::io::BufReader::new(body_reader(&hs, body));
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                upload::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
            })
            .await
        }
    };
    respond_first(
        "application/x-git-upload-pack-result",
        &app,
        (&o, &n),
        run_protocol,
        body,
        repo,
    )
    .await
}

async fn receive_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, false).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let store = app.store.clone();
    let hs = headers.clone();
    let run_protocol = move |repo: Repo, body: Bytes| {
        let (store, flag, hs) = (store.clone(), flag.clone(), hs.clone());
        async move {
            let mut input = std::io::BufReader::new(body_reader(&hs, body));
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                receive::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
            })
            .await
        }
    };
    respond_first(
        "application/x-git-receive-pack-result",
        &app,
        (&o, &n),
        run_protocol,
        body,
        repo,
    )
    .await
}

type Joined = std::result::Result<crate::Result<Vec<u8>>, tokio::task::JoinError>;

/// Turn the first attempt into a response, and on a fence that routing says we may still own, run
/// it once more against a freshly opened handle. The body is `Bytes`, so that is a plain second
/// call.
async fn respond_first<F, Fut>(
    ct: &'static str,
    app: &App,
    (o, n): (&str, &str),
    run_protocol: F,
    body: Bytes,
    repo: Repo,
) -> Response
where
    F: Fn(Repo, Bytes) -> Fut,
    Fut: std::future::Future<Output = Joined>,
{
    let res = match run_protocol(repo, body.clone()).await {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    match res {
        Ok(out) => success(ct, out),
        // See App::on_fenced. If routing still says we own it, reopen and run the request again.
        Err(e) if crate::pool::is_fenced(&e) => match reopen_after_fence(app, o, n).await {
            None => fenced_elsewhere(),
            Some(repo) => match run_protocol(repo, body).await {
                Ok(Ok(out)) => success(ct, out),
                // a second fence is a real error, not retried again
                Ok(Err(e)) => internal(e),
                Err(e) => internal(crate::err(e.to_string())),
            },
        },
        Err(e) => internal(e),
    }
}

fn success(ct: &'static str, out: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        out,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router and `BROWSE_TAILS` are two lists that must agree. A route in
    /// one and not the other is unreachable (registered but refused by the
    /// middleware) or unroutable (allowed through to a 404), and neither says
    /// which. Read the routes out of the source and compare.
    #[test]
    fn every_browse_route_is_routable() {
        let src = include_str!("http/browse_api.rs");
        let mut tails: Vec<&str> = src
            .split("\"/api/{owner}/{name}/")
            .skip(1)
            .filter_map(|rest| rest.split(['/', '"']).next())
            .filter(|t| !t.is_empty())
            .collect();
        tails.sort_unstable();
        tails.dedup();
        assert!(!tails.is_empty(), "found no routes to check");
        for tail in tails {
            assert!(
                BROWSE_TAILS.contains(&tail),
                "browse_routes registers `{tail}` but BROWSE_TAILS does not list it, so the \
                 routing middleware answers 404 before the router ever runs",
            );
        }
    }

    #[test]
    fn an_api_path_is_only_ever_a_browse_route() {
        // `api` is a reserved owner, so this is `alice/info`'s refs — the same repo axum's router
        // dispatches it to — and never the git route of a repo `api/alice`.
        assert!(!git_shape("/api/alice/info/refs"));
        assert_eq!(api_route("/api/alice/info/refs"), Some(("alice", "info")));
        assert_eq!(repo_of("/api/alice/info/refs"), Some("alice/info".into()));
        assert_eq!(repo_of("/api/alice/web/tree/abc/src"), Some("alice/web".into()));
        // An `/api/` path that is not a browse route is not routable at all. `repo_of` says None
        // and `route_inner` REFUSES it — it must never fall through to matchit, which would match
        // `/{owner}/{name}/git-upload-pack` with owner=`api`. See `api_prefixed`.
        assert!(api_prefixed("/api/alice/git-upload-pack"));
        assert!(api_route("/api/alice/git-upload-pack").is_none());
        assert_eq!(repo_of("/api/alice/git-upload-pack"), None);
        assert!(!crate::store::valid_owner("api"));
    }

    #[test]
    fn non_api_paths_are_unchanged() {
        assert_eq!(repo_of("/alice/web/info/refs"), Some("alice/web".into()));
        assert_eq!(repo_of("/alice/web.git/git-upload-pack"), Some("alice/web".into()));
        assert_eq!(repo_of("/healthz"), None);
        assert!(!is_git_route("/healthz"));
        assert!(!is_git_route("/alice/web/nope"));
    }
}
