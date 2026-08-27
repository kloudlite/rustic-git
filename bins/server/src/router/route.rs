use super::limits::internal;
use crate::App;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rustic_git_core::httpx::Trusted;
use std::sync::Arc;

/// Liveness/readiness. 503 when the object store has stopped answering.
pub(crate) async fn healthz(State(app): State<Arc<App>>) -> Response {
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
pub(crate) async fn own_claim(State(app): State<Arc<App>>, body: String) -> Response {
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

pub(crate) async fn own_renew(State(app): State<Arc<App>>, body: String) -> Response {
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

pub(crate) async fn own_release(State(app): State<Arc<App>>, body: String) -> Response {
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
pub(crate) async fn own_draining(State(app): State<Arc<App>>, body: String) -> Response {
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

/// The final path segment of a git route (`/{owner}/{name}/{tail}`).
pub(crate) const GIT_ROUTE_TAILS: [&str; 3] = ["info", "git-upload-pack", "git-receive-pack"];

/// The third segment of a browse route (`/api/{owner}/{name}/{tail}`). Every entry is repo-scoped
/// and peer-only. `visibility`, `create` and `description` are the WRITES among them (all POST), which is why
/// they belong here rather than in a separate list — they must be routed to the owner exactly as
/// the reads are, so the node that serves the repo is the node that writes it.
///
/// A route missing from this list is UNREACHABLE — the middleware refuses it
/// before the router ever sees it — so adding a browse route means adding its
/// tail here. `every_browse_route_is_routable` holds the two together.
///
/// `imagetags`, `imagetagdelete`, `imagedelete`, `imagevisibility`, `volumehistory` and
/// `volumedelete` are
/// repo-scoped like the rest (though the first four route by the IMAGE key and the last by the
/// VOLUME key — see `repo_of`). `images` and `volumes` are the two owner-scoped exceptions — see
/// `api_route`.
pub(crate) const BROWSE_TAILS: [&str; 25] = [
    "refs", "tree", "blob", "log", "commit", "files", "lastmod", "compare", "signature",
    "visibility", "create", "description", "delete", "protect", "merge", "patch", "images", "imagetags",
    "imagetagdelete", "imagedelete", "imagevisibility",
    // `volumes` is owner-scoped like `images` (two segments, no name); `volumehistory` names a
    // VOLUME and routes by the volume key, below.
    "volumes", "volumehistory", "volumedelete",
    // Every pull-request route — list, get, comment, merge, close, check — has `pulls` as its
    // third segment, so this one entry covers all of them.
    "pulls",
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
pub(crate) fn api_prefixed(path: &str) -> bool {
    path.trim_start_matches('/') == "api"
        || path.trim_start_matches('/').starts_with("api/")
}

/// Whether this path is a git route (`/{owner}/{name}/{info|git-upload-pack|git-receive-pack}`).
/// Never true under `/api/`, per `api_prefixed`.
pub(crate) fn git_shape(path: &str) -> bool {
    if api_prefixed(path) {
        return false;
    }
    let mut it = path.trim_start_matches('/').split('/');
    let (Some(_), Some(_), Some(tail)) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    GIT_ROUTE_TAILS.contains(&tail)
}

/// `Some((owner, name, tail))` when the path is a browse route. `name` and `tail` are both `""` for
/// the two owner-scoped routes, `images` and `volumes` (`/api/{owner}/images`, two segments, no
/// repo name) — every other tail is repo-scoped (`/api/{owner}/{name}/{tail}`, three segments), and
/// `tail` is that third segment, which `repo_of` needs to tell the routes that key by an IMAGE
/// (`imagetags`) or a VOLUME (`volumehistory`) apart from those that key by the repo.
pub(crate) fn api_route(path: &str) -> Option<(&str, &str, &str)> {
    let mut it = path.trim_start_matches('/').strip_prefix("api/")?.split('/');
    let owner = it.next()?;
    let second = it.next()?;
    match it.next() {
        Some(tail) => BROWSE_TAILS.contains(&tail).then_some((owner, second, tail)),
        None => BROWSE_TAILS.contains(&second).then_some((owner, "", "")),
    }
}

pub(crate) fn repo_of(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    if crate::registry::is_v2_path(path) {
        let (owner, name) = crate::registry::image_route(path)?;
        return Some(crate::registry::routing_key(owner, name));
    }
    // `/vol-agent/{owner}/{name}/{tail}` names a VOLUME, a third keyspace beside repos and
    // images: `repo/vol/{owner}/{name}`, one keyspace over from `repo/img/{owner}/{name}`. Same
    // shape as the `/v2/` branch above — a separate routing key, checked before the git branches
    // so a volume path is never mistaken for a repo of the same owner/name.
    if crate::vol_agent::vol_agent_prefixed(path) {
        let (owner, name) = crate::vol_agent::vol_agent_route(path)?;
        return Some(rustic_git_workspaces::registry::routing_key(owner, name));
    }
    // `/api/{owner}/{name}/...` names a repo exactly as the git routes do; skipping the `api`
    // segment makes both shapes yield the same repo, so both route to the same node. An `/api/`
    // path resolves through `api_route` and nothing else — see `api_prefixed`.
    if api_prefixed(path) {
        let (owner, name, tail) = api_route(path)?;
        // `images` has no repo to route by: it only reads the shared object store, so there is
        // nothing to forward to a particular node — `None` here means "served locally" in
        // `route_inner`, exactly like `/healthz`.
        if name.is_empty() {
            return None;
        }
        // `imagetags`, `imagetagdelete` and `imagedelete` all name an IMAGE, not a repo: the image
        // database is keyed `img/{owner}/{name}`, a different key (and potentially a different
        // node) than the git repo of the same name. Route by the image key so this reaches the
        // node that actually owns that database.
        if matches!(tail, "imagetags" | "imagetagdelete" | "imagedelete" | "imagevisibility") {
            return Some(crate::registry::routing_key(owner, name));
        }
        // `volumehistory` names a VOLUME — `vol/{owner}/{name}`, the third keyspace — for the same
        // reason `imagetags` names an image: the records live in that database and only the node
        // holding it may open it. `/api/` and `/vol-agent/` therefore route to the same node for
        // the same volume, which is what lets one of them read what the other wrote.
        if matches!(tail, "volumehistory" | "volumedelete") {
            return Some(rustic_git_workspaces::registry::routing_key(owner, name));
        }
        let (owner, name) = crate::protocol::parse_repo_pair(owner, name)?;
        return Some(format!("{owner}/{name}"));
    }
    let mut it = path.split('/');
    let (owner, name, rest) = (it.next()?, it.next()?, it.next()?);
    if !GIT_ROUTE_TAILS.contains(&rest) {
        return None;
    }
    let (owner, name) = crate::protocol::parse_repo_pair(owner, name)?;
    Some(format!("{owner}/{name}"))
}

/// Whether this path has the shape of a git route (`/{owner}/{name}/{info|git-upload-pack|git-receive-pack}`),
/// regardless of whether the segments parse. `route` uses this to tell "not ours to route" from
/// "ours, but malformed": the latter must be refused, never passed to a handler that would decode
/// it and open a repo this node does not own.
pub(crate) fn is_git_route(path: &str) -> bool {
    // `/api/{owner}/{name}/...` is repo-scoped exactly as the git routes are: it must reach the
    // owner, because only the owner holds the database and the packs. `images` is the exception —
    // an empty `name` means there is no repo to reach, so it is not a git route (`repo_of` already
    // answers `None` for it, and that `None` must mean "serve locally", not "malformed").
    git_shape(path)
        || matches!(api_route(path), Some((_, name, _)) if !name.is_empty())
        || crate::registry::image_route(path).is_some()
}

/// Route before handling. Runs ahead of authentication: the damage is done by *opening* a repo's
/// database on the wrong node, so a misrouted request must never reach the handlers. Applied to
/// both listeners — a node receiving a forwarded request consults its own copy of the map (and the
/// leader, if that copy has nothing), bounded by the hop count.
/// Public listener: `/api/...` is not repo-scoped here, so it is never forwarded.
pub(crate) async fn route_public(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    route_inner(app, req, next, false).await
}

/// Peer listener: the browse API lives here, so `/api/...` routes like any other repo path.
pub(crate) async fn route_peer(
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
        if crate::registry::LOCAL_V2.contains(&tail) {
            return next.run(req).await;
        }
        return crate::registry::oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image");
    }
    // A `/vol-agent/` path that names no volume is neither routable nor a registered route:
    // refuse here, exactly as the `/v2/` branch above does for its own prefix. Falling through
    // would let a path with the right SHAPE but an invalid owner/name (`repo_of` -> `None`) reach
    // the `None => next.run(req).await` arm below and be served locally, having never routed.
    if crate::vol_agent::vol_agent_prefixed(&path) && crate::vol_agent::vol_agent_route(&path).is_none() {
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
                                        tracing::error!(repo = %repo, peer = %peer.name, error = %again, "forwarding");
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
                                        tracing::warn!(repo = %repo, error = %e, "force-claiming");
                                    }
                                }
                            }
                            // The leader is unreachable, or refused: answer as before.
                            Err(e) => tracing::warn!(repo = %repo, error = %e, "claim after failed forward"),
                        }
                    }
                    tracing::error!(repo = %repo, peer = %peer.name, error = %e, "forwarding");
                    (StatusCode::BAD_GATEWAY, "peer error").into_response()
                }
            }
        }
    }
}

/// Peer listener admission: the secret, then the identity the caller established.
pub(crate) async fn trust_peer(
    State(app): State<Arc<App>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = req
        .headers()
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !crate::proxy::secret_eq(presented, &app.forwarder.secret) {
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
pub(crate) async fn trust_nobody(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The router and `BROWSE_TAILS` are two lists that must agree. A route in
    /// one and not the other is unreachable (registered but refused by the
    /// middleware) or unroutable (allowed through to a 404), and neither says
    /// which. Read the routes out of the source and compare.
    ///
    /// Two shapes are scraped: the repo-scoped `/api/{owner}/{name}/{tail}` (most routes) and the
    /// owner-scoped `/api/{owner}/{tail}` (`images` alone, today). Checking only the first shape
    /// left `images` unverified — present in `BROWSE_TAILS` but nothing would catch it being
    /// removed — so both shapes are asserted here.
    #[test]
    fn every_browse_route_is_routable() {
        let src = include_str!("../browse_api/mod.rs");
        let mut tails: Vec<&str> = src
            .split("\"/api/{owner}/{name}/")
            .skip(1)
            .filter_map(|rest| rest.split(['/', '"']).next())
            .filter(|t| !t.is_empty())
            .collect();
        let owner_scoped: Vec<&str> = src
            .split("\"/api/{owner}/")
            .skip(1)
            .filter_map(|rest| rest.split(['/', '"']).next())
            .filter(|t| !t.is_empty() && *t != "{name}")
            .collect();
        // Each shape must actually be found — a scrape that silently extracts nothing from one
        // shape (say, the registration format changes) would otherwise pass vacuously, which is
        // exactly what let `imagetags`'s routing bug through review.
        assert!(!tails.is_empty(), "found no repo-scoped (`/api/{{owner}}/{{name}}/...`) routes");
        assert!(!owner_scoped.is_empty(), "found no owner-scoped (`/api/{{owner}}/...`) routes");
        assert!(tails.contains(&"refs"), "expected `refs` among the repo-scoped tails");
        assert!(
            owner_scoped.contains(&"images"),
            "expected `images` among the owner-scoped tails"
        );
        tails.extend(owner_scoped);
        tails.sort_unstable();
        tails.dedup();
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
        assert_eq!(api_route("/api/alice/info/refs"), Some(("alice", "info", "refs")));
        assert_eq!(repo_of("/api/alice/info/refs"), Some("alice/info".into()));
        assert_eq!(repo_of("/api/alice/web/tree/abc/src"), Some("alice/web".into()));
        // `imagetags` is the one repo-scoped tail that routes by the IMAGE key, not the repo key:
        // the image database is keyed `img/{owner}/{name}`, which may live on a different node than
        // the git repo of the same name.
        assert_eq!(
            repo_of("/api/alice/web/imagetags"),
            Some(crate::registry::routing_key("alice", "web")),
        );
        assert_ne!(repo_of("/api/alice/web/imagetags"), repo_of("/api/alice/web/refs"));
        // `volumes` is the second owner-scoped route: no repo to reach, so `None` means "serve
        // here", exactly as it does for `images`. It reads the shared object store alone, which is
        // the only reason that is safe.
        assert_eq!(api_route("/api/alice/volumes"), Some(("alice", "", "")));
        assert_eq!(repo_of("/api/alice/volumes"), None);
        // `volumehistory` names a VOLUME — a third keyspace, and a potentially different node than
        // either the repo or the image of that name.
        assert_eq!(
            repo_of("/api/alice/ws-1/volumehistory"),
            Some(rustic_git_workspaces::registry::routing_key("alice", "ws-1")),
        );
        assert_ne!(repo_of("/api/alice/ws-1/volumehistory"), repo_of("/api/alice/ws-1/refs"));
        // The delete routes by the same volume key: it opens the same database the history does,
        // so it has to reach the same node.
        assert_eq!(repo_of("/api/alice/ws-1/volumedelete"), repo_of("/api/alice/ws-1/volumehistory"));
        // An `/api/` path that is not a browse route is not routable at all. `repo_of` says None
        // and `route_inner` REFUSES it — it must never fall through to matchit, which would match
        // `/{owner}/{name}/git-upload-pack` with owner=`api`. See `api_prefixed`.
        assert!(api_prefixed("/api/alice/git-upload-pack"));
        assert!(api_route("/api/alice/git-upload-pack").is_none());
        assert_eq!(repo_of("/api/alice/git-upload-pack"), None);
        assert!(!crate::store::valid_owner("api"));
    }

    /// `VOL_AGENT_TAILS` and the routes actually mounted in `vol_agent.rs` are two lists that
    /// must agree, for the same reason `every_browse_route_is_routable` checks `BROWSE_TAILS`
    /// against `browse_api/mod.rs`: a tail missing from the list is unreachable (the middleware
    /// 404s it before the router runs), and one missing from the router is a route nothing serves.
    #[test]
    fn every_vol_agent_route_is_routable() {
        let src = include_str!("../vol_agent.rs");
        let mut tails: Vec<&str> = src
            .split("\"/vol-agent/{owner}/{name}/")
            .skip(1)
            .filter_map(|rest| rest.split(['"']).next())
            .filter(|t| !t.is_empty())
            .collect();
        assert!(!tails.is_empty(), "found no `/vol-agent/{{owner}}/{{name}}/...` routes");
        tails.sort_unstable();
        tails.dedup();
        for tail in tails {
            assert!(
                crate::vol_agent::VOL_AGENT_TAILS.contains(&tail),
                "vol_agent_routes registers `{tail}` but VOL_AGENT_TAILS does not list it, so \
                 the routing middleware answers 404 before the router ever runs",
            );
        }
    }

    #[test]
    fn a_vol_agent_path_routes_by_the_volume_key_not_the_repo_key() {
        assert_eq!(
            repo_of("/vol-agent/alice/web/commits"),
            Some(rustic_git_workspaces::registry::routing_key("alice", "web")),
        );
        assert_ne!(
            repo_of("/vol-agent/alice/web/commits"),
            repo_of("/alice/web/info/refs"),
        );
        // Shape matches but the owner is reserved (`vol` itself): unroutable, not silently local.
        assert_eq!(repo_of("/vol-agent/vol/web/commits"), None);
        assert!(crate::vol_agent::vol_agent_prefixed("/vol-agent/vol/web/commits"));
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
