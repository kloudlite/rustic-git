//! Forwarding a request to the node the ownership map names as the owner.
//!
//! Two forwarding shapes, because the two client protocols are not the same shape. An HTTP request
//! is one request and one response, so it is reverse-proxied. An SSH session is a stream carrying
//! an advertisement and then repeated commands, so it is piped (see `stream`).

use crate::Result;
use std::time::Duration;

/// Identity of the client the *forwarding* node authenticated. Honoured only on the peer listener.
pub const OWNER_HEADER: &str = "x-rustic-git-owner";
/// How many times this request has been forwarded. Bounds re-forwarding.
pub const HOPS_HEADER: &str = "x-rustic-git-hops";
/// Shared secret on every peer request. The peer ports are separate and unpublished, but this
/// cluster runs with `networkPolicy: none`, so any pod can reach them; this is defence in depth on
/// top of the port, not instead of it.
pub const PEER_HEADER: &str = "x-rustic-git-peer";
/// A forward happens only when a node's copy of the map is behind, and the leader corrects that
/// in one round trip — so two hops is already slack. Past this, refuse rather than bounce.
pub const MAX_HOPS: u32 = 2;

/// Constant-time peer-secret compare, shared by every site that checks one (api.rs `caller`,
/// http.rs `trust_peer`, the stream check below). A byte-by-byte `!=` on a shared secret leaks
/// its prefix through early-exit timing; an empty secret must never authenticate anyone, even
/// against an empty presented value, so both sides are guarded here rather than at each call site.
pub fn secret_eq(presented: &str, expected: &str) -> bool {
    if presented.is_empty() || expected.is_empty() || presented.len() != expected.len() {
        return false;
    }
    presented.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Connecting to a peer inside the cluster is a microsecond round trip; a second is three orders
/// of magnitude of headroom, and a peer that has not accepted by then is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a claim/renew/release waits on the leader. It is one small write behind a 10ms flush,
/// so this is generous — but bounded, because a request is blocked on it.
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether this failure was "could not reach the peer at all", as opposed to anything the client's
/// own behaviour could produce. Only the former may trigger a re-route.
///
/// `crate::Error` is `Box<dyn Error>`; `forward`'s `?` boxes the `reqwest::Error` without erasing
/// its concrete type, so downcasting recovers it. Using `reqwest::Error::is_connect()` instead of
/// matching on the message text means this keeps working across reqwest versions that reword it.
pub fn is_connect_error(e: &crate::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect())
}
/// A claim rides out a leader restart: attempts x backoff must exceed how long the leader is away
/// during a roll (~35s measured), while staying under a git client's patience.
pub const CLAIM_ATTEMPTS: u32 = 20;
pub const CLAIM_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1500);
/// A recovery ask — the owner just failed to answer — gets two quick tries and then a fast 502.
/// The client had a working owner a moment ago and can retry; thirty seconds of waiting on a
/// leader that is also away (a rolling restart) is the regression this bound prevents.
pub const RECOVER_ATTEMPTS: u32 = 2;
pub const RECOVER_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);
/// A release retries too, but briefly: it runs while the node is shutting down, and attempts x
/// backoff must stay well inside the release budget in `serve()`.
pub const RELEASE_ATTEMPTS: u32 = 4;
pub const RELEASE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(400);

/// Headers that describe one hop, not the message. Forwarded verbatim they mislead the next hop:
/// git sends `Expect: 100-continue` on pushes over 1 MiB, and `Transfer-Encoding` describes *our*
/// framing, not the peer's. Stripped in both directions; each hop frames its own body.
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
    "transfer-encoding", "upgrade", "expect", "content-length", "host",
];

pub struct Forwarder {
    pub(crate) client: reqwest::Client,
    pub(crate) secret: String,
}

#[cfg(test)]
mod is_connect_error_tests {
    use super::is_connect_error;

    /// A real connect failure (nothing listening on this port) must classify as recoverable.
    #[tokio::test]
    async fn connect_failure_is_recoverable() {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap();
        // Port 0 is never a listener; the OS refuses the connect immediately.
        let err = client.get("http://127.0.0.1:0/").send().await.unwrap_err();
        let boxed: crate::Error = Box::new(err);
        assert!(is_connect_error(&boxed));
    }

    /// An error that is not even a `reqwest::Error` must not be misclassified as a connect
    /// failure — the old string-match on "error sending request" could accidentally hit unrelated
    /// text; the downcast cannot.
    #[test]
    fn non_reqwest_error_is_not_connect_error() {
        let boxed: crate::Error = crate::err("connection refused, sort of");
        assert!(!is_connect_error(&boxed));
    }
}

impl Forwarder {
    pub fn new(secret: String) -> Forwarder {
        Forwarder {
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                // No total timeout: a clone of a large repo legitimately streams for a long time.
                .build()
                .expect("building an HTTP client cannot fail with these options"),
            secret,
        }
    }

    /// Send this request to `addr` and stream its response back, one hop further along.
    pub async fn forward(
        &self,
        addr: &str,
        owner: &str,
        hops: u32,
        req: axum::extract::Request,
    ) -> Result<axum::response::Response> {
        use axum::body::{Body, HttpBody};
        let (parts, body) = req.into_parts();
        let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        // A HEAD reply carries the length of the entity it describes and no body to frame, so the
        // hop-by-hop rule below must not apply to it on the way back — see the response loop.
        let head = parts.method == axum::http::Method::HEAD;
        // A body of known length is re-framed with its own Content-Length rather than left to
        // fall back to chunked: the length is exact here (it did not change hop to hop), and
        // avoiding chunked keeps a small request looking like a small request on the wire.
        let exact_len = body.size_hint().exact();
        let mut headers = parts.headers.clone();
        for h in HOP_BY_HOP {
            headers.remove(*h);
        }
        if let Some(len) = exact_len {
            headers.insert(axum::http::header::CONTENT_LENGTH, len.into());
        }
        headers.insert(OWNER_HEADER, owner.parse()?);
        headers.insert(HOPS_HEADER, (hops + 1).to_string().parse()?);
        headers.insert(PEER_HEADER, self.secret.parse()?);

        let upstream = self
            .client
            .request(parts.method, format!("http://{addr}{path}"))
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;

        let mut out = axum::response::Response::builder().status(upstream.status());
        for (k, v) in upstream.headers() {
            // `content-length` is hop-by-hop going out because each hop frames its own body. Coming
            // back that only holds when there IS a body to re-frame: a HEAD has none, so dropping
            // the header does not defer to our framing, it destroys the single number the client
            // asked for. Clients then fall back to a full GET on every manifest probe.
            let keep = head && k == axum::http::header::CONTENT_LENGTH;
            if keep || !HOP_BY_HOP.contains(&k.as_str()) {
                out = out.header(k, v);
            }
        }
        Ok(out.body(Body::from_stream(upstream.bytes_stream()))?)
    }
}


// ---- The stream side: forwarded SSH sessions, piped byte for byte. ----

use crate::App;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const HEADER_MAX: usize = 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// The stream port sits one above the HTTP peer port on every node.
/// ponytail: fixed offset; make it configurable if the ports ever need to be independent.
pub fn stream_addr(http_peer: &str) -> String {
    let (host, port) = http_peer
        .rsplit_once(':')
        .expect("peer address must be host:port — it is built by this program, never by input");
    let port: u16 = port
        .parse()
        .expect("peer port must be numeric — it is built by this program, never by input");
    format!("{host}:{}", port + 1)
}

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
                eprintln!("peer stream: {e}"); // ponytail: eprintln; swap for a logger when one exists
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
    if hops >= MAX_HOPS && !matches!(route, crate::ownership::Route::Local) {
        return refuse(reader, "routing disagreement at hop limit; retry").await;
    }
    if hops < MAX_HOPS {
        match route {
            crate::ownership::Route::Local => {}
            crate::ownership::Route::Unavailable => {
                return refuse(reader, "no node may safely serve this repository; retry").await
            }
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
    let repo = match app.store.open_repo(&ro, &rn).await {
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

/// Pipe an established stream to the node that owns the repo, one hop further along.
///
/// Sends the header, waits for the owner's status line, then copies bytes both ways. With `relay`,
/// the caller is a middle node: the owner's status is written upstream (as "ok" or "error: …")
/// before piping starts, so an upstream node waiting on a status line gets one — and never gets
/// "error:" after "ok", because after "ok" nothing but git bytes is ever written upstream.
///
/// Borrows the stream so the caller keeps it alive afterwards: on the SSH path the stream *is* the
/// channel, and dropping it closes the channel — but the exit status has to go out first. `run` in
/// ssh.rs makes the same point about its own bridges.
#[allow(clippy::too_many_arguments)]
pub async fn stream_to_peer<S>(
    secret: &str,
    peer_stream: &str,
    service: &str,
    repo: &str,
    owner: &str,
    hops: u32,
    stream: &mut S,
    relay: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let connect_and_status = async {
        let sock = tokio::net::TcpStream::connect(peer_stream).await?;
        let mut sock = BufReader::new(sock);
        sock.get_mut()
            .write_all(format!("{secret} {service} {repo} {owner} {}\n", hops + 1).as_bytes())
            .await?;
        // The owner answers "ok" after validating the header, before it opens the repo, so this
        // wait is short in practice — but bounded generously rather than by HEADER_TIMEOUT, since
        // the owner may itself be routing (one claim to the leader) before it answers.
        let mut status = String::new();
        // Grows with hops remaining: each downstream node may itself route and wait on its own downstream, so the edge must outwait the whole chain.
        let wait = Duration::from_secs(30) * (MAX_HOPS - hops.min(MAX_HOPS) + 1);
        tokio::time::timeout(wait, sock.read_line(&mut status)).await??;
        let status = status.trim_end().to_string();
        if status != "ok" {
            return Err(crate::err(
                status.strip_prefix("error: ").unwrap_or(&status).to_string(),
            ));
        }
        Ok::<_, crate::Error>(sock)
    };
    let mut sock = match connect_and_status.await {
        Ok(s) => s,
        Err(e) => {
            if relay {
                let _ = stream.write_all(format!("error: {e}\n").as_bytes()).await;
            }
            return Err(e);
        }
    };
    if relay {
        stream.write_all(b"ok\n").await?; // the node upstream is waiting on this
    }
    // Both directions until either side finishes; copy_bidirectional half-closes the write side on
    // EOF, which is what git expects. NOTE: on an SSH channel stream, this shutdown already sends
    // the channel EOF (russh ChannelStream::poll_shutdown) — the caller must not send a second one.
    tokio::io::copy_bidirectional(stream, &mut sock).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::secret_eq;

    #[test]
    fn secret_eq_rejects_empty_and_mismatched() {
        assert!(!secret_eq("", ""));
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "ab"));
    }
}
