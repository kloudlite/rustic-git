//! The browser terminal: `/term/{bench}/…` proxied to ttyd on `BENCH_TERM_PORT` in the bench pod.
//!
//! Same claims-then-resolve-then-dial shape as `tunnel.rs`, but ttyd is plain HTTP/WebSocket, not
//! a raw byte pipe: the first GET carries `?token=` (the 60s bench-session token minted by
//! `POST /v1/bench/session`), and every request after that carries the `kl_term` cookie instead —
//! a `<iframe>` has no way to set a header, so the cookie IS the credential past the first load.
//!
//! The `Path=/term/{bench}/` scope keeps a cookie minted for one bench from being sent on a
//! request to another bench's path, and `claims.bench == bench` is checked again on every request
//! regardless — the cookie's path scope is the browser's courtesy, never the gate.
//!
// ponytail: the 60 s session token bounds page load + WS connect; a reconnect re-opens from the
// console; mint a longer term token if reconnects annoy.

use crate::resolve::resolve_bench;
use crate::tunnel::Gateway;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use kloudlite_core::jwt::BenchSessionClaims;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TtMsg;

const COOKIE: &str = "kl_term";
/// ttyd buffers a whole HTTP reply before answering; its own assets are a few KiB, so this is
/// generous headroom, not a tuning knob.
const MAX_HTTP_REPLY: usize = 1 << 20;
/// ttyd's own replies are tiny and local; this bounds a peer that ignores `Connection: close`
/// rather than tuning for a slow one.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(serde::Deserialize)]
struct TermQuery {
    token: Option<String>,
}

pub fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/term/{bench}/", get(http_root))
        .route("/term/{bench}/ws", get(ws_proxy))
        .route("/term/{bench}/{*rest}", get(http_proxy))
}

/// The one check every request makes: a token (query, on the first load, else the cookie) that
/// names THIS bench. Every refusal is a plain status — this is a console page, not a credential
/// a retried request should keep secret the reason for.
fn authorize(gw: &Gateway, bench: &str, headers: &HeaderMap, query: Option<&str>) -> Result<BenchSessionClaims, StatusCode> {
    let token = query.map(str::to_string).or_else(|| cookie_token(headers)).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = gw.jwt.verify_bench_session(&token).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if claims.bench != bench || claims.region != gw.region {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(claims)
}

/// `Origin`'s host must match `Host`: refuses a cross-site WS upgrade while still allowing the
/// same ttyd origin that opens this connection (no `Origin` header at all, e.g. a non-browser
/// client, is let through — there is nothing to compare).
fn same_host_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let origin_host = origin.split("://").nth(1).unwrap_or(origin);
    origin_host == host
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}

/// A `bench` or `rest` path segment that could smuggle something into the raw HTTP/1.1 request
/// line or headers this module hand-writes to ttyd (`proxy_http` below) — axum percent-decodes
/// `{*rest}` before we see it, so `%0D%0A` already landed as a real CRLF by the time it gets here.
fn unsafe_path_segment(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || c.is_whitespace() || c == '%') || s.split('/').any(|seg| seg == "..")
}

/// The console (`dev.kloudlite.io`) and the terminal (`ws-{region}.khost.dev`) are different
/// sites, so the iframe is a third-party context: `SameSite=Strict` is never sent there, and
/// `SameSite=None` alone would still be third-party-cookie-blocked. CHIPS (`Partitioned`)
/// partitions the cookie jar by the embedding page's top-level site, which is what makes a
/// same-origin-with-the-iframe, cross-site-with-the-console cookie work at all. `Max-Age` is
/// capped at the token's own `exp` so the cookie never outlives the credential it carries.
fn set_cookie(bench: &str, token: &str, exp: u64) -> (axum::http::HeaderName, String) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let max_age = exp.saturating_sub(now);
    (
        axum::http::header::SET_COOKIE,
        format!("{COOKIE}={token}; Path=/term/{bench}/; Max-Age={max_age}; HttpOnly; Secure; SameSite=None; Partitioned"),
    )
}

async fn http_root(state: State<Arc<Gateway>>, path: Path<String>, headers: HeaderMap, query: Query<TermQuery>) -> Response {
    let Path(bench) = path;
    proxy_http(state, bench, String::new(), headers, query).await
}

async fn http_proxy(state: State<Arc<Gateway>>, path: Path<(String, String)>, headers: HeaderMap, query: Query<TermQuery>) -> Response {
    let Path((bench, rest)) = path;
    proxy_http(state, bench, rest, headers, query).await
}

/// GET-only reverse proxy over a plain TCP HTTP/1.1 request: ttyd serves `/`, `/token` and a
/// handful of static assets, nothing that needs a body or a second verb.
async fn proxy_http(State(gw): State<Arc<Gateway>>, bench: String, rest: String, headers: HeaderMap, Query(q): Query<TermQuery>) -> Response {
    if unsafe_path_segment(&bench) || unsafe_path_segment(&rest) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let claims = match authorize(&gw, &bench, &headers, q.token.as_deref()) {
        Ok(c) => c,
        Err(s) => return s.into_response(),
    };
    let target = match resolve_bench(&gw.kube, &bench, gw.term_port).await {
        Ok(t) => t,
        Err((status, why)) => {
            tracing::debug!(bench = %bench, reason = why, "term.resolve.failed");
            return status.into_response();
        }
    };
    let mut tcp = match TcpStream::connect(target.addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(bench = %bench, error = %e, "term.dial.failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let req = format!("GET /{rest} HTTP/1.1\r\nHost: term\r\nConnection: close\r\n\r\n");
    if tcp.write_all(req.as_bytes()).await.is_err() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut raw = Vec::new();
    // `Connection: close` above is what makes reading to EOF the end of the reply rather than a
    // hang — no chunked-encoding or Content-Length parsing needed for ttyd's small, static replies.
    // The timeout catches a peer that ignores `Connection: close`; the length check catches a
    // reply that hit the cap and was silently truncated by `take`.
    let read = tokio::time::timeout(UPSTREAM_READ_TIMEOUT, tcp.take(MAX_HTTP_REPLY as u64).read_to_end(&mut raw)).await;
    if !matches!(read, Ok(Ok(_))) || raw.len() >= MAX_HTTP_REPLY {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let Some((head, body)) = split_once_crlf2(&raw) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .and_then(|c| StatusCode::from_u16(c).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = lines
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-type")).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_else(|| "application/octet-stream".into());
    let mut resp = Response::builder().status(status).header(axum::http::header::CONTENT_TYPE, content_type);
    // Only the first GET of the page carries `?token=`; a later request already riding the cookie
    // has one set and nothing to refresh. The cookie carries the SAME token (never a fresh mint),
    // so it expires exactly when the bench-session token does.
    if let Some(t) = &q.token {
        let (name, val) = set_cookie(&bench, t, claims.exp);
        resp = resp.header(name, val);
    }
    match resp.body(axum::body::Body::from(body.to_vec())) {
        Ok(r) => r,
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn split_once_crlf2(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (&raw[..i], &raw[i + 4..]))
}

async fn ws_proxy(State(gw): State<Arc<Gateway>>, Path(bench): Path<String>, headers: HeaderMap, Query(q): Query<TermQuery>, upgrade: WebSocketUpgrade) -> Response {
    if unsafe_path_segment(&bench) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Checked before authorize: the `kl_term` cookie is `SameSite=None` (CHIPS-partitioned, but
    // not every browser honours `Partitioned` yet), so without this a third-party page could open
    // this socket riding the cookie alone — a writable shell into the agent's session. No console
    // origin setting exists yet, so same-host is the only thing to compare against.
    if !same_host_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(status) = authorize(&gw, &bench, &headers, q.token.as_deref()) {
        return status.into_response();
    }
    let target = match resolve_bench(&gw.kube, &bench, gw.term_port).await {
        Ok(t) => t,
        Err((status, why)) => {
            tracing::debug!(bench = %bench, reason = why, "term.ws.resolve.failed");
            return status.into_response();
        }
    };
    let mut req = match format!("ws://{}/ws", target.addr).into_client_request() {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    // ttyd's own protocol name; without it ttyd refuses the handshake.
    req.headers_mut().insert("sec-websocket-protocol", axum::http::HeaderValue::from_static("tty"));
    let (upstream, _) = match tokio_tungstenite::connect_async(req).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(bench = %bench, error = %e, "term.ws.dial.failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    upgrade.protocols(["tty"]).on_upgrade(move |sock| pump(sock, upstream))
}

async fn pump(client: WebSocket, upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>) {
    use futures::{SinkExt, StreamExt};
    let (mut ctx, mut crx) = client.split();
    let (mut utx, mut urx) = upstream.split();
    loop {
        tokio::select! {
            msg = crx.next() => match msg {
                Some(Ok(AxMsg::Binary(b))) => if utx.send(TtMsg::Binary(b)).await.is_err() { break },
                Some(Ok(AxMsg::Text(t))) => if utx.send(TtMsg::Text(t.to_string().into())).await.is_err() { break },
                Some(Ok(AxMsg::Close(_))) | None => { let _ = utx.send(TtMsg::Close(None)).await; break }
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            msg = urx.next() => match msg {
                Some(Ok(TtMsg::Binary(b))) => if ctx.send(AxMsg::Binary(b)).await.is_err() { break },
                Some(Ok(TtMsg::Text(t))) => if ctx.send(AxMsg::Text(t.as_str().to_string().into())).await.is_err() { break },
                Some(Ok(TtMsg::Close(_))) | None => { let _ = ctx.send(AxMsg::Close(None)).await; break }
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}
