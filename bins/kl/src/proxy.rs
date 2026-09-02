//! `kl ws proxy <id>` — ssh's ProxyCommand. Everything on this path is opaque ssh bytes; the only
//! thing that must never appear in output is the session token.

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// The session `kl ws ssh` minted, handed down through ssh's environment so this child makes no
/// api call of its own.
pub const SESSION_ENV: &str = "KL_SSH_SESSION";

pub async fn proxy(id: &str) -> Result<(), String> {
    // A session from the parent `kl ws ssh` is used as is (host key already pinned there). The
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
                    return Err("your login has expired — run `kl login`".to_string())
                }
                Err(e) => return Err(e.to_string()),
            };
            crate::config::pin_host_key(id, &s.host_key)?;
            s
        }
    };
    pump(&gateway_url(&s.gateway), &s.token).await
}

/// `KL_GATEWAY_OVERRIDE` (hidden, tests and e2e only) swaps the origin of the api-supplied gateway
/// URL, keeping its path — so the pump can be exercised against a local server without the api
/// having to know about it.
fn gateway_url(gateway: &str) -> String {
    let Ok(origin) = std::env::var("KL_GATEWAY_OVERRIDE") else {
        return gateway.to_string();
    };
    let path = gateway
        .split_once("://")
        .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or(""))
        .unwrap_or("");
    format!("{}{path}", origin.trim_end_matches('/'))
}

async fn pump(url: &str, token: &str) -> Result<(), String> {
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

    let (mut tx, mut rx) = ws.split();

    // stdin lives in its own task: ssh reads and writes independently, and a read that blocks the
    // write half deadlocks the handshake.
    let up = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    });

    let mut stdout = tokio::io::stdout();
    while let Some(msg) = rx.next().await {
        match msg {
            Ok(Message::Binary(b)) => {
                stdout.write_all(&b).await.map_err(|e| e.to_string())?;
                // ssh is a request/response handshake: an unflushed reply is a hang.
                stdout.flush().await.map_err(|e| e.to_string())?;
            }
            Ok(Message::Close(_)) => break,
            // A dropped tunnel is not a clean end of session: ssh must see a failure, and the
            // error kind (never the token, which appears in no frame) is the one line printed.
            Err(e) => {
                up.abort();
                return Err(format!("tunnel error: {e}"));
            }
            Ok(_) => {}
        }
    }
    up.abort();
    Ok(())
}
