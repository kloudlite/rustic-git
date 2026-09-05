//! Forwarding a request to the node the ownership map names as the owner.
//!
//! Two forwarding shapes, because the two client protocols are not the same shape. An HTTP request
//! is one request and one response, so it is reverse-proxied. An SSH session is a stream carrying
//! an advertisement and then repeated commands, so it is piped (see `stream`).

pub use kloudlite_core::peer::*;

use crate::Result;
use std::time::Duration;

// ---- The stream side: forwarded SSH sessions, piped byte for byte. ----

use crate::App;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const HEADER_MAX: usize = 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// Accept forwarded SSH sessions.
///
/// One header line, then one status line back, then the git protocol byte for byte. The socket is
/// then handed to the same `serve_git` a local SSH client reaches, so nothing about the protocol is
/// reimplemented here — which is the point of piping rather than translating.
pub async fn serve_peer_streams(app: Arc<App>, listener: TcpListener) -> Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_peer_stream(app, sock).await {
                tracing::warn!(error = %e, "peer.stream.failed");
            }
        });
    }
}

async fn serve_peer_stream(app: Arc<App>, sock: tokio::net::TcpStream) -> Result<()> {
    let mut reader = BufReader::new(sock);
    // Bounded and timed: a stray connection that never sends a newline must not hold a task or
    // grow a buffer without limit.
    let mut header = Vec::new();
    let n = tokio::time::timeout(
        HEADER_TIMEOUT,
        (&mut reader).take(HEADER_MAX as u64).read_until(b'\n', &mut header),
    )
    .await??;
    if n == 0 || header.last() != Some(&b'\n') {
        return Err(crate::err("peer stream: bad header")); // silently closed
    }
    let header = String::from_utf8_lossy(&header).trim_end().to_string();
    let mut parts = header.splitn(5, ' ');
    // Secret first, checked before anything else is parsed. Wrong: close without a byte.
    let presented = parts.next().unwrap_or_default();
    if !secret_eq(presented, &app.forwarder.secret) {
        return Err(crate::err("peer stream: secret"));
    }
    let service = parts.next().unwrap_or_default().to_string();
    let repo = parts.next().unwrap_or_default().to_string();
    let owner = parts.next().unwrap_or_default().to_string();
    // Unparseable hops = exhausted: serve here rather than bounce.
    let hops: u32 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(MAX_HOPS);

    // From here on, refusals are reported as a status line: the forwarding node relays them so
    // the client sees a reason and a non-zero exit, as it would from a local session.
    // `&'static str`: an annotated `&str` here would be higher-ranked and the returned future could
    // not capture it. Every reason is a literal, so 'static is honest.
    let refuse = |reader: BufReader<tokio::net::TcpStream>, why: &'static str| async move {
        let mut s = reader.into_inner();
        let _ = s.write_all(format!("error: {why}\n").as_bytes()).await;
        Err::<(), crate::Error>(crate::err(why))
    };
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return refuse(reader, "unsupported service").await;
    }
    if !crate::store::valid_segment(&owner) {
        return refuse(reader, "invalid owner").await;
    }
    let Some((ro, rn)) = crate::protocol::parse_repo_path(&repo) else {
        return refuse(reader, "invalid repo path").await;
    };
    // The forwarding node authenticated the client; this node still decides what that identity may
    // reach. Trusting who the caller says it is is not the same as skipping authorisation.
    // A peer always presents an identity, so the public flag can never change this outcome.
    if !crate::auth::authorize(Some(owner.as_str()), &ro, false) {
        return refuse(reader, "access denied").await;
    }
    // Same rule as HTTP: consult the map from here, forward on if it names someone else — unless
    // out of hops, where we still refuse to serve what routing says is not ours.
    let route = app.route(&format!("{ro}/{rn}")).await;
    // `Missing` counts as ours to answer: there is nothing under the key, so serving here opens
    // nothing and the session reports it after the identity is already established.
    if hops >= MAX_HOPS
        && !matches!(
            route,
            crate::ownership::Route::Local | crate::ownership::Route::Missing
        )
    {
        return refuse(reader, "routing disagreement at hop limit; retry").await;
    }
    if hops < MAX_HOPS {
        match route {
            crate::ownership::Route::Local => {}
            crate::ownership::Route::Unavailable => {
                return refuse(reader, "no node may safely serve this repository; retry").await
            }
            // Nothing under this key: serve here, and let the session report it exactly as it
            // does for a repo that was deleted. Nothing is opened — `open_repo` checks first.
            crate::ownership::Route::Missing => {}
            crate::ownership::Route::Peer(peer) => {
                // Two-hop: we are the middle node. stream_to_peer reads the OWNER's status line
                // itself; with `relay = true` it writes a status line UPSTREAM to the node that
                // forwarded to us — "ok" once the owner said ok, or "error: …" if the owner refused
                // — BEFORE piping, so it can never write "error:" after "ok". Keep the BufReader:
                // any bytes it buffered past the header belong to git.
                let mut sock = reader;
                return stream_to_peer(
                    &app.forwarder.secret,
                    &stream_addr(&peer.addr),
                    &service,
                    &format!("{ro}/{rn}"),
                    &owner,
                    hops,
                    &mut sock,
                    true,
                )
                .await;
            }
        }
    }
    // "ok" goes out BEFORE open_repo. Opening a cold repo downloads its packs — seconds to
    // minutes for a big one — and the forwarding node is waiting on this line under a short
    // timeout meant for the header exchange, not for a pack download. Once "ok" is sent, a
    // missing repo is reported the way a local session reports it: on the git ERR channel with a
    // non-zero exit, which git prints as-is.
    let mut sock = reader; // BufReader kept: see above
    sock.get_mut().write_all(b"ok\n").await?;
    let repo = match app.open_repo_after_fence(&ro, &rn).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            let _ = crate::pktline::write_err(&mut sock, "repository not found").await;
            return Err(crate::err("repository not found"));
        }
        Err(e) if crate::pool::is_fenced(&e) => {
            let _ = crate::pktline::write_err(&mut sock, "repository moved; retry").await;
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    crate::ssh::serve_git(app.store.clone(), repo, &service, sock).await
}
