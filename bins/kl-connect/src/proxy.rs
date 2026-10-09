//! `kl-connect ws proxy <id>` — ssh's ProxyCommand. Everything on this path is opaque ssh bytes; the only
//! thing that must never appear in output is the session token.

use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// The session `kl-connect ws ssh` minted, handed down through ssh's environment so this child makes no
/// api call of its own.
pub const SESSION_ENV: &str = "KL_SSH_SESSION";

pub async fn proxy(id: &str) -> Result<(), String> {
    // A session from the parent `kl-connect ws ssh` is used as is (host key already pinned there). The
    // mint stays for the `ssh-config` blocks, where ssh runs this with no `kl` parent at all.
    let handed = std::env::var(SESSION_ENV)
        .ok()
        .and_then(|v| serde_json::from_str::<crate::api::Session>(&v).ok())
        .filter(|s| s.id == id);
    let s = match handed {
        Some(s) => s,
        None => {
            let cfg = crate::config::load()?;
            let s = match crate::api::ssh_session(&cfg, id).await {
                Ok(s) => s,
                // No retry: the second attempt would send the same stored token, so a 401 is a
                // fact about the token, not a transient. Say what fixes it.
                Err(crate::api::Error::Unauthorized) => {
                    return Err("your login has expired — run `kl-connect login`".to_string())
                }
                Err(e) => return Err(e.to_string()),
            };
            crate::config::pin_host_key(id, &s.host_key)?;
            s
        }
    };
    let ws = connect(&gateway_url(&s.gateway), &s.token).await?;
    pump_io(ws, tokio::io::stdin(), tokio::io::stdout(), None).await
}

/// `KL_GATEWAY_OVERRIDE` (hidden, tests and e2e only) swaps the origin of the api-supplied gateway
/// URL, keeping its path — so the pump can be exercised against a local server without the api
/// having to know about it.
pub(crate) fn gateway_url(gateway: &str) -> String {
    let Ok(origin) = std::env::var("KL_GATEWAY_OVERRIDE") else {
        return gateway.to_string();
    };
    let path = gateway
        .split_once("://")
        .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or(""))
        .unwrap_or("");
    format!("{}{path}", origin.trim_end_matches('/'))
}

/// Opens the tunnel and authenticates it. Split from `pump_io` so `bench.rs` can wait for a
/// sleeping bench and mint its own session before dialling.
pub(crate) async fn connect(
    url: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("{url}: {e}"))?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "bad session token".to_string())?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        // The token travels in this request, so it must not survive into the error text.
        .map_err(|e| format!("gateway unreachable: {}", e.to_string().replace(token, "…")))?;
    Ok(ws)
}

/// Pumps binary frames between an open tunnel and any reader/writer pair: stdio for `ws proxy`
/// and `bench-proxy`, an in-memory pipe in tests. Writes are flushed per frame (a request/response
/// handshake on the other end), and a `Close` frame ends the pump cleanly while any other error is
/// reported.
///
/// `keepalive`: `bench-proxy` has no server-side keepalive behind it, so it Pings every interval
/// to keep the Cloudflare edge (100 s idle) awake and gives up after three with no frame of any
/// kind back (3 x 15 s).
pub(crate) async fn pump_io<
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
>(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    mut r: R,
    mut w: W,
    keepalive: Option<Duration>,
) -> Result<(), String> {
    let (mut tx, mut rx) = ws.split();
    // The reader lives in its own task: writer and reader progress independently, and a read
    // that blocks would deadlock the write half. The Pings go out from here too, so the write
    // half has one owner.
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        let mut tick = keepalive.map(tokio::time::interval);
        loop {
            let ping = async {
                match tick.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                n = r.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                },
                _ = ping => {
                    if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    });

    let limit = keepalive.map(|k| k * 3);
    loop {
        let next = match limit {
            Some(l) => match tokio::time::timeout(l, rx.next()).await {
                Ok(m) => m,
                Err(_) => {
                    up.abort();
                    return Err("lost the bench connection".to_string());
                }
            },
            None => rx.next().await,
        };
        let Some(msg) = next else { break };
        match msg {
            Ok(Message::Binary(b)) => {
                w.write_all(&b).await.map_err(|e| e.to_string())?;
                w.flush().await.map_err(|e| e.to_string())?;
            }
            Ok(Message::Close(_)) => break,
            // A dropped tunnel is not a clean end of session: the caller must see a failure, and
            // the error kind (never the token, which appears in no frame) is the one line printed.
            Err(e) => {
                up.abort();
                return Err(format!("tunnel error: {e}"));
            }
            Ok(_) => {} // Pong and anything else: already counted as a sign of life by the timeout
        }
    }
    up.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway that upgrades and then never sends a frame or answers a Ping.
    async fn silent() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(s).await.unwrap();
            // hold the socket open, read nothing: tungstenite only answers Pings while read
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(ws);
        });
        format!("ws://{addr}/tui/b")
    }

    /// A gateway that reads (so tungstenite answers Pings) but sends no data.
    async fn quiet() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            while let Some(Ok(_)) = futures::StreamExt::next(&mut ws).await {}
        });
        format!("ws://{addr}/tui/b")
    }

    #[tokio::test]
    async fn no_pong_ends_the_pump_with_lost_connection() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ws = connect(&silent().await, "t").await.unwrap();
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let ka = std::time::Duration::from_millis(50);
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), pump_io(ws, r, w, Some(ka)))
            .await
            .expect("pump must give up on its own")
            .unwrap_err();
        assert_eq!(err, "lost the bench connection");
    }

    #[tokio::test]
    async fn pongs_keep_a_quiet_connection_alive() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ws = connect(&quiet().await, "t").await.unwrap();
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let ka = std::time::Duration::from_millis(50);
        // 10 x the keepalive with no data: well past the 3x limit, so only Pongs keep it up
        let res = tokio::time::timeout(std::time::Duration::from_millis(500), pump_io(ws, r, w, Some(ka))).await;
        assert!(res.is_err(), "pump ended early: {res:?}");
    }
}
