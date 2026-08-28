//! The tunnel: authorize, resolve, dial, pump.
//!
//! Everything the gateway decides happens BEFORE the upgrade, so a refusal is a plain HTTP status
//! the CLI can print. After `101` the gateway is a pipe: it holds no credential, reads no ssh
//! frame, and cannot open a session of its own — sshd still wants the user's key.

use crate::resolve::resolve;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rustic_git_core::jwt::Jwt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The edge idles a WebSocket after 100s without traffic and sshd's `ClientAliveInterval 30` keeps
/// it under that, so half an hour of silence means the client is gone, not quiet.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// 64 KiB: an ssh packet is at most 32 KiB, so a frame never has to be split for size.
const MAX_FRAME: usize = 64 * 1024;
const MAX_PER_WS: usize = 10;
const MAX_PER_OWNER: usize = 100;

pub struct Gateway {
    pub jwt: Jwt,
    pub region: String,
    pub kube: kube::Client,
    /// 22 everywhere real; a test points it at a local echo listener.
    pub ssh_port: u16,
    /// Spent session ids → their expiry. A token is a CONNECT token: replaying one is either a
    /// bug or an attack, and both are refused the same way.
    // ponytail: per-replica, so a replayed token could still connect to a different replica within
    // its 60s life. Global single-use needs Redis; the TTL is the real mitigation.
    used: Mutex<HashMap<String, u64>>,
    // ponytail: per-replica counters, not global — with one replica per node and a per-IP rate
    // limit at the edge in front, the ceiling is "N nodes × the limit". Redis if that matters.
    per_ws: Mutex<HashMap<String, usize>>,
    per_owner: Mutex<HashMap<String, usize>>,
}

impl Gateway {
    pub fn new(jwt: Jwt, region: String, kube: kube::Client, ssh_port: u16) -> Gateway {
        Gateway {
            jwt,
            region,
            kube,
            ssh_port,
            used: Mutex::new(HashMap::new()),
            per_ws: Mutex::new(HashMap::new()),
            per_owner: Mutex::new(HashMap::new()),
        }
    }

    /// `true` the first time a session id is seen, `false` ever after. Expired entries are swept
    /// on the way in — the map is bounded by the connect rate over one token TTL, not by uptime.
    fn spend(&self, jti: &str, exp: u64) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        used.retain(|_, e| *e > now);
        used.insert(jti.to_string(), exp).is_none()
    }
}

/// A live tunnel's place in both counters, released by dropping it — including on every early
/// return between the reservation and the pump, which is where a leaked count would come from.
struct Slot {
    gw: Arc<Gateway>,
    ws: String,
    owner: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(&self.gw.per_ws, &self.ws);
        release(&self.gw.per_owner, &self.owner);
    }
}

fn release(map: &Mutex<HashMap<String, usize>>, key: &str) {
    let mut m = map.lock().unwrap_or_else(|p| p.into_inner());
    match m.get_mut(key) {
        Some(n) if *n > 1 => *n -= 1,
        // Drop the entry at zero rather than leaving it: the map would otherwise grow one entry
        // per workspace ever connected to and never shrink.
        _ => {
            m.remove(key);
        }
    }
}

fn take(map: &Mutex<HashMap<String, usize>>, key: &str, limit: usize) -> bool {
    let mut m = map.lock().unwrap_or_else(|p| p.into_inner());
    let n = m.entry(key.to_string()).or_insert(0);
    if *n >= limit {
        return false;
    }
    *n += 1;
    true
}

fn reserve(gw: &Arc<Gateway>, ws: &str, owner: &str) -> Option<Slot> {
    if !take(&gw.per_ws, ws, MAX_PER_WS) {
        return None;
    }
    let slot = Slot { gw: gw.clone(), ws: ws.into(), owner: owner.into() };
    // The workspace count is already held by `slot`, so failing here still releases it on drop.
    take(&gw.per_owner, owner, MAX_PER_OWNER).then_some(slot)
}

pub fn app(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/tunnel/{ws}", get(tunnel))
        .with_state(gw)
}

async fn tunnel(
    State(gw): State<Arc<Gateway>>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Every refusal below is the same 401 on purpose: which of the checks failed is the caller's
    // business only insofar as "get a new token", and saying more distinguishes a real workspace
    // from an invented one for someone holding a token for neither.
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let claims = match gw.jwt.verify_ssh_session(token) {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // A token names ONE workspace in ONE region. The region check is what stops a token minted
    // for another region's gateway being replayed here against a workspace that shares an id.
    if claims.ws != ws || claims.region != gw.region {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !gw.spend(&claims.jti, claims.exp) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let target = match resolve(&gw.kube, &ws, gw.ssh_port).await {
        Ok(t) => t,
        Err((status, why)) => {
            tracing::info!(ws = %ws, why, "refused");
            return status.into_response();
        }
    };
    // Counted against the workspace's OWNER, not the token's subject: on a team workspace those
    // differ, and the limit is about one tenant's fan-out, not one person's. Authorization is not
    // re-derived from either — the api checked `may_act_on` at mint, and this token names exactly
    // one workspace, so a token that reaches here can reach nothing else.
    let Some(slot) = reserve(&gw, &ws, &target.owner) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // Dial BEFORE the upgrade, so a pod that is not listening is a 502 the CLI can print rather
    // than a WebSocket that opens and immediately closes for no stated reason.
    let tcp = match tokio::net::TcpStream::connect(target.addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(ws = %ws, error = %e, "dial failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    upgrade
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |sock| pump(sock, tcp, slot))
        .into_response()
}

async fn pump(sock: WebSocket, mut tcp: tokio::net::TcpStream, slot: Slot) {
    use futures::{SinkExt, StreamExt};
    // Split so the two directions borrow different halves: `select!` holds both futures alive
    // while a branch body runs, and sending from the TCP branch would otherwise alias the socket.
    let (mut tx, mut rx) = sock.split();
    let start = Instant::now();
    let (mut r#in, mut out) = (0u64, 0u64);
    let mut buf = vec![0u8; MAX_FRAME];
    loop {
        // One timeout around the whole select, restarted each iteration: any frame in either
        // direction is what resets the idle clock, which is the definition we want.
        let step = tokio::time::timeout(IDLE, async {
            tokio::select! {
                msg = rx.next() => match msg {
                    Some(Ok(Message::Binary(b))) => {
                        r#in += b.len() as u64;
                        tcp.write_all(&b).await.is_ok()
                    }
                    Some(Ok(Message::Close(_))) => false,
                    // Ping/Pong are answered by axum; a text frame is not something an ssh client
                    // sends, so it is ignored rather than treated as an error.
                    Some(Ok(_)) => true,
                    _ => false,
                },
                n = tcp.read(&mut buf) => match n {
                    Ok(0) | Err(_) => false,
                    Ok(n) => {
                        out += n as u64;
                        tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_ok()
                    }
                },
            }
        })
        .await;
        if !matches!(step, Ok(true)) {
            break;
        }
    }
    // Never the token, and never a byte of the stream: this line is the whole record of a session.
    tracing::info!(
        owner = %slot.owner,
        ws = %slot.ws,
        bytes_in = r#in,
        bytes_out = out,
        secs = start.elapsed().as_secs(),
        "tunnel closed"
    );
}
