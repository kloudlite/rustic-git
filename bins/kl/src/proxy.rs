//! `kl ws proxy <id>` — ssh's ProxyCommand. Everything on this path is opaque ssh bytes; the only
//! thing that must never appear in output is the session token.

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub async fn proxy(id: &str) -> Result<(), String> {
    let cfg = crate::config::load()?;
    let s = match crate::api::ssh_session(&cfg, id).await {
        Ok(s) => s,
        // One retry: a 401 is worth a second attempt before telling someone to log in again, and
        // the mint is cheap.
        Err(crate::api::Error::Unauthorized) => crate::api::ssh_session(&cfg, id)
            .await
            .map_err(|_| "your login has expired — run `kl login`".to_string())?,
        Err(e) => return Err(e.to_string()),
    };
    crate::config::pin_host_key(id, &s.host_key)?;
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
    let mut req = url.into_client_request().map_err(|e| format!("{url}: {e}"))?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().map_err(|_| "bad session token".to_string())?,
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
                    if tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
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
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    up.abort();
    Ok(())
}
