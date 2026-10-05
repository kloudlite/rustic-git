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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TtMsg;

const COOKIE: &str = "kl_term";
/// ttyd buffers a whole HTTP reply before answering; its own assets are a few KiB, so this is
/// generous headroom, not a tuning knob.
const MAX_HTTP_REPLY: usize = 1 << 20;

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

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}

/// `Path=/term/{bench}/` so a cookie minted for one bench is never sent on a request to another's
/// path; `HttpOnly`/`Secure`/`SameSite=Strict` because this is a bearer credential, not a UI
/// preference. Set only on the query-token path — a request already riding the cookie has nothing
/// to re-set.
fn set_cookie(bench: &str, token: &str) -> (axum::http::HeaderName, String) {
    (axum::http::header::SET_COOKIE, format!("{COOKIE}={token}; Path=/term/{bench}/; HttpOnly; Secure; SameSite=Strict"))
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
    if tcp.take(MAX_HTTP_REPLY as u64).read_to_end(&mut raw).await.is_err() {
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
    let _ = &claims;
    if let Some(t) = &q.token {
        let (name, val) = set_cookie(&bench, t);
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
    if authorize(&gw, &bench, &headers, q.token.as_deref()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
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
