//! Binds the four listeners `serve()` needs: public HTTP, SSH, peer HTTP, and the peer stream
//! port (`proxy::stream_addr`, derived from the peer address). Split out purely because these
//! four `bind` calls used to sit in the middle of `serve()`'s much longer setup.

use crate::config::env;
use crate::Result;
use tokio::net::TcpListener;

pub struct Listeners {
    pub http: TcpListener,
    pub ssh: TcpListener,
    pub peer_http: TcpListener,
    pub peer_stream: TcpListener,
}

pub async fn bind(peer_addr: &str) -> Result<Listeners> {
    let http = TcpListener::bind(env("KLOUDLITE_GIT_HTTP_ADDR", "0.0.0.0:8080")).await?;
    let ssh = TcpListener::bind(env("KLOUDLITE_GIT_SSH_ADDR", "0.0.0.0:2222")).await?;
    let peer_http = TcpListener::bind(peer_addr).await?;
    let peer_stream = TcpListener::bind(crate::proxy::stream_addr(peer_addr)).await?;
    Ok(Listeners { http, ssh, peer_http, peer_stream })
}
