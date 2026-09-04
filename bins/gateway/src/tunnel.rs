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
use kloudlite_git_core::jwt::Jwt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The edge idles a WebSocket after 100s without traffic and sshd's `ClientAliveInterval 30` keeps
/// it under that, so half an hour of silence means the client is gone, not quiet.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// 64 KiB: an ssh packet is at most 32 KiB, so a frame never has to be split for size.
const MAX_FRAME: usize = 64 * 1024;
const MAX_PER_WS: usize = 10;
const MAX_PER_OWNER: usize = 100;
/// The per-owner limit times the number of owners is unbounded, and a tunnel is ~100 KiB of
/// buffers; this is what keeps the pod inside its memory limit when everyone reconnects at once.
const MAX_TUNNELS: usize = 1000;

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
    tunnels: Arc<Semaphore>,
    /// Env-only until `main.rs`'s boot-time GET and refresh beat store into it — this binary
    /// today reads nothing from `CentralSettings` (no field feeds a gateway tunable yet), so the
    /// handle exists only for `/healthz`'s version and for parity with the other three central
    /// binaries.
    pub central:
        kloudlite_git_core::settings::LiveSettings<kloudlite_git_core::settings::CentralSettings>,
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
            tunnels: Arc::new(Semaphore::new(MAX_TUNNELS)),
            central: kloudlite_git_core::settings::LiveSettings::new(
                kloudlite_git_core::settings::CentralSettings::from_env(),
            ),
        }
    }

    /// Reserve a place for a tunnel to `ws`: the global cap and the per-workspace count. The
    /// owner is not known until the workspace is resolved, so that count is charged afterwards
    /// by `Slot::charge`. Reserving BEFORE resolving is what keeps a reconnect storm against one
    /// workspace from becoming a storm against the API server.
    fn reserve(self: &Arc<Self>, ws: &str) -> Option<Slot> {
        let permit = self.tunnels.clone().try_acquire_owned().ok()?;
        if !take(&self.per_ws, ws, MAX_PER_WS) {
            return None;
        }
        metrics::gauge!("gateway_open_tunnels").increment(1.0);
        Some(Slot { gw: self.clone(), ws: ws.into(), owner: None, _permit: permit })
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

/// A live tunnel's place in every counter, released by dropping it — including on every early
/// return between the reservation and the pump, which is where a leaked count would come from.
/// Each count is released exactly when it was taken: `owner` is `Some` only once the per-owner
/// `take` succeeded, so a refused connect can never decrement a count it never held.
struct Slot {
    gw: Arc<Gateway>,
    ws: String,
    owner: Option<String>,
    _permit: OwnedSemaphorePermit,
}

impl Slot {
    fn charge(&mut self, owner: &str) -> bool {
        let ok = take(&self.gw.per_owner, owner, MAX_PER_OWNER);
        if ok {
            self.owner = Some(owner.into());
        }
        ok
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        metrics::gauge!("gateway_open_tunnels").decrement(1.0);
        release(&self.gw.per_ws, &self.ws);
        if let Some(owner) = &self.owner {
            release(&self.gw.per_owner, owner);
        }
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

pub fn app(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|State(gw): State<Arc<Gateway>>| async move {
                format!("ok settings={}", gw.central.version())
            }),
        )
        .route("/tunnel/{ws}", get(tunnel))
        .layer(axum::middleware::from_fn_with_state("gateway", kloudlite_git_core::metrics::http_metrics))
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
    let token = kloudlite_git_core::httpx::bearer_token(&headers).unwrap_or_default();
    let claims = match gw.jwt.verify_ssh_session(token) {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // A token names ONE workspace in ONE region. The region check is what stops a token minted
    // for another region's gateway being replayed here against a workspace that shares an id.
    if claims.ws != ws || claims.region != gw.region {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(mut slot) = gw.reserve(&ws) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let target = match resolve(&gw.kube, &ws, gw.ssh_port).await {
        Ok(t) => t,
        Err((status, why)) => {
            tracing::debug!(workspace = %ws, reason = why, "tunnel.refused");
            return status.into_response();
        }
    };
    // Counted against the workspace's OWNER, not the token's subject: on a team workspace those
    // differ, and the limit is about one tenant's fan-out, not one person's. Authorization is not
    // re-derived from either — the api checked `may_act_on` at mint, and this token names exactly
    // one workspace, so a token that reaches here can reach nothing else.
    if !slot.charge(&target.owner) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // Dial BEFORE the upgrade, so a pod that is not listening is a 502 the CLI can print rather
    // than a WebSocket that opens and immediately closes for no stated reason.
    let tcp = match tokio::net::TcpStream::connect(target.addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(workspace = %ws, error = %e, "tunnel.dial.failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    // ssh is interactive: a keystroke must not wait for Nagle to batch it with the next one.
    let _ = tcp.set_nodelay(true);
    // Spent only now that the connect can actually proceed: a 409 (still starting), a 503 (at a
    // connection limit) and a 502 (pod not listening yet) are the refusals worth retrying, and
    // burning the token on any of them would turn a retryable refusal into "log in again".
    // Everything after this point either upgrades or fails for a reason a new token cannot fix.
    if !gw.spend(&claims.jti, claims.exp) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade
        .max_frame_size(MAX_FRAME)
        // Both, not just the frame: a peer may FRAGMENT one message across many frames, and axum
        // buffers the whole thing before yielding it — 1024 conforming 64 KiB frames would
        // assemble 64 MiB against a 128Mi pod.
        .max_message_size(MAX_FRAME)
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
                    // sshd hung up. Say so rather than dropping the socket: a bare TCP close
                    // reaches the CLI as a protocol error, a Close frame as a finished session.
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Message::Close(None)).await;
                        false
                    }
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
        owner = slot.owner.as_deref().unwrap_or_default(),
        workspace = %slot.ws,
        bytes_in = r#in,
        bytes_out = out,
        duration_ms = start.elapsed().as_millis() as u64,
        "tunnel.closed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw() -> Arc<Gateway> {
        let (client, _) = kloudlite_git_workspaces::kube_test::mock_client(vec![]);
        Arc::new(Gateway::new(Jwt::new("0123456789abcdef0123456789abcdef").unwrap(), "r".into(), client, 22))
    }

    fn count(map: &Mutex<HashMap<String, usize>>, key: &str) -> usize {
        map.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    #[tokio::test]
    async fn a_slot_dropped_on_any_exit_path_leaves_every_count_at_zero() {
        let gw = gw();
        // Dropped before the owner was charged (resolve failed) and after (dial failed).
        drop(gw.reserve("ws-1").unwrap());
        let mut s = gw.reserve("ws-1").unwrap();
        assert!(s.charge("alice"));
        drop(s);
        assert_eq!(count(&gw.per_ws, "ws-1"), 0);
        assert_eq!(count(&gw.per_owner, "alice"), 0);
        assert_eq!(gw.tunnels.available_permits(), MAX_TUNNELS);
    }

    #[tokio::test]
    async fn a_refused_owner_charge_does_not_release_a_live_tunnels_count() {
        let gw = gw();
        let live: Vec<Slot> = (0..MAX_PER_OWNER)
            .map(|i| {
                let mut s = gw.reserve(&format!("ws-{i}")).unwrap();
                assert!(s.charge("alice"));
                s
            })
            .collect();
        // Many refusals: each must leave the owner exactly as full as it was.
        for _ in 0..3 * MAX_PER_OWNER {
            let mut s = gw.reserve("ws-x").unwrap();
            assert!(!s.charge("alice"));
        }
        assert_eq!(count(&gw.per_owner, "alice"), MAX_PER_OWNER);
        drop(live);
        assert_eq!(count(&gw.per_owner, "alice"), 0);
    }

    #[tokio::test]
    async fn spent_session_ids_are_swept_once_expired() {
        let gw = gw();
        for i in 0..10_000 {
            assert!(gw.spend(&format!("old-{i}"), 1));
        }
        assert!(gw.spend("live", u64::MAX));
        assert!(!gw.spend("live", u64::MAX));
        assert_eq!(gw.used.lock().unwrap().len(), 1);
    }
}
