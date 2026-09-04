//! The client half of forwarding a request to the node the ownership map names as the owner.

use crate::Result;
use std::time::Duration;

/// Identity of the client the *forwarding* node authenticated. Honoured only on the peer listener.
pub const OWNER_HEADER: &str = "x-kloudlite-git-owner";
/// How many times this request has been forwarded. Bounds re-forwarding.
pub const HOPS_HEADER: &str = "x-kloudlite-git-hops";
/// Shared secret on every peer request. The peer ports are separate and unpublished, but this
/// cluster runs with `networkPolicy: none`, so any pod can reach them; this is defence in depth on
/// top of the port, not instead of it.
pub const PEER_HEADER: &str = "x-kloudlite-git-peer";
/// A forward happens only when a node's copy of the map is behind, and the leader corrects that
/// in one round trip — so two hops is already slack. Past this, refuse rather than bounce.
pub const MAX_HOPS: u32 = 2;

/// Constant-time peer-secret compare, shared by every site that checks one (api.rs `caller`,
/// `router/route.rs` `trust_peer`, the stream check below). A byte-by-byte `!=` on a shared secret leaks
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
    pub client: reqwest::Client,
    pub secret: String,
}

#[cfg(test)]
mod is_connect_error_tests {
    use super::is_connect_error;

    /// A real connect failure (nothing listening on this port) must classify as recoverable.
    #[tokio::test]
    async fn connect_failure_is_recoverable() {
        // reqwest's TLS feature is `rustls-no-provider`: the binaries install ring at start, a
        // bare unit test has to do the same or `build()` fails before any connect is attempted.
        let _ = rustls::crypto::ring::default_provider().install_default();
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
