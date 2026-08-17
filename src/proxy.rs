//! Forwarding a request to the node that owns the repo, and the probes routing needs.
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
/// Candidates are three deep, so two forwards reach the last of them. Past this, serve here.
pub const MAX_HOPS: u32 = 2;

/// A probe must distinguish "down" from "slow". Both vantages time out for one cause if the owner
/// is merely busy — a GC pause, a burst of requests — and the owner, as top candidate, checks
/// nobody; two vantages agreeing on a timeout is not two independent observations. So probes are
/// generous and retried once, and a positive answer is cached briefly so a hot owner is probed at
/// most once per second per node, not once per request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_RETRIES: u32 = 1;
const PROBE_CACHE: Duration = Duration::from_secs(1);

/// Headers that describe one hop, not the message. Forwarded verbatim they mislead the next hop:
/// git sends `Expect: 100-continue` on pushes over 1 MiB, and `Transfer-Encoding` describes *our*
/// framing, not the peer's. Stripped in both directions; each hop frames its own body.
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
    "transfer-encoding", "upgrade", "expect", "content-length", "host",
];

async fn probe_via_once(client: &reqwest::Client, secret: &str, via_addr: &str, target_name: &str) -> Option<bool> {
    let r = client
        .get(format!("http://{via_addr}/probe"))
        .query(&[("peer", target_name)])
        .header(PEER_HEADER, secret)
        .timeout(PROBE_TIMEOUT * (PROBE_RETRIES + 1) + Duration::from_secs(1))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None; // includes 503 = the via is unhealthy; its word is not a vantage
    }
    match r.text().await.ok()?.trim() {
        "up" => Some(true),
        "down" => Some(false),
        _ => None, // includes "unknown": the via does not know that peer (stale view)
    }
}

async fn probe_once_with_retry(client: &reqwest::Client, secret: &str, addr: &str) -> bool {
    for _ in 0..=PROBE_RETRIES {
        let r = client.get(format!("http://{addr}/healthz"))
            .header(PEER_HEADER, secret).timeout(PROBE_TIMEOUT).send().await;
        match r {
            Ok(r) if r.status().is_success() => return true,
            // A definite answer that is not 200 (403 = wrong secret, 503 = unhealthy) is "down"
            // without retry; only a timeout or connect failure earns the retry.
            Ok(_) => return false,
            Err(e) if e.is_timeout() || e.is_connect() => continue,
            Err(_) => return false,
        }
    }
    false
}

pub struct Forwarder {
    pub(crate) client: reqwest::Client,
    pub(crate) secret: String,
    /// Recent positive probes, so a hot owner is not probed on every request.
    up_cache: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// In-flight probes by address, so N concurrent requests for a dead owner's repos share one
    /// probe rather than issuing N. Negatives are still never cached — this only dedups probes
    /// that are happening right now.
    in_flight: std::sync::Mutex<std::collections::HashMap<String, futures::future::Shared<futures::future::BoxFuture<'static, bool>>>>,
    /// Same, for second-vantage requests, keyed "via|target".
    via_in_flight: std::sync::Mutex<std::collections::HashMap<String, futures::future::Shared<futures::future::BoxFuture<'static, Option<bool>>>>>,
}

impl Forwarder {
    pub fn new(secret: String) -> Forwarder {
        Forwarder {
            client: reqwest::Client::builder()
                .connect_timeout(PROBE_TIMEOUT)
                // No total timeout: a clone of a large repo legitimately streams for a long time.
                .build()
                .expect("building an HTTP client cannot fail with these options"),
            secret,
            up_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            in_flight: std::sync::Mutex::new(std::collections::HashMap::new()),
            via_in_flight: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Whether a peer's *application* answers right now.
    ///
    /// `GET /healthz` with the secret, expecting 200. Not a bare TCP connect: a pod mid-shutdown
    /// still accepts TCP for a moment before it dies, and treating that as reachable is how two
    /// nodes end up holding one repo. The secret matters too — the peer listener refuses requests
    /// without it, and a refused probe would read as "down" for every peer, collapsing routing to
    /// every node serving everything.
    ///
    /// Retried once on timeout, because "slow" must not read as "down" (see PROBE_TIMEOUT). A
    /// positive answer is cached for PROBE_CACHE; a negative one never is — only fresh evidence may
    /// demote a peer.
    pub async fn reachable(&self, addr: &str) -> bool {
        if let Some(at) = self.up_cache.lock().unwrap().get(addr) {
            if at.elapsed() < PROBE_CACHE { return true; }
        }
        // Single-flight: if a probe of this address is already running, await that one.
        let fut = {
            let mut m = self.in_flight.lock().unwrap();
            if let Some(f) = m.get(addr) {
                f.clone()
            } else {
                use futures::FutureExt;
                let client = self.client.clone();
                let secret = self.secret.clone();
                let a = addr.to_string();
                let f = async move { probe_once_with_retry(&client, &secret, &a).await }.boxed().shared();
                m.insert(addr.to_string(), f.clone());
                f
            }
        };
        let up = fut.await;
        self.in_flight.lock().unwrap().remove(addr);
        if up {
            self.up_cache.lock().unwrap().insert(addr.to_string(), std::time::Instant::now());
        }
        up
    }

    /// The second vantage: can `via` reach `target`? `None` if `via` itself did not answer, or
    /// does not know the target — neither is evidence about `target` either way.
    ///
    /// Single-flight per (via, target), like `reachable`: a failover decision asks
    /// |above| × |others| vias concurrently, and N concurrent requests for a dead owner's repos
    /// would otherwise fan that out N×. The via side already dedups its real /healthz probe via
    /// its own `reachable`, so this only bounds the asking node's outbound HTTP.
    ///
    /// The timeout here must EXCEED the via's own probe budget: `/probe` runs `reachable()`, which
    /// is PROBE_TIMEOUT plus one retry when the target is blackholed (a crashed pod's IP, still in
    /// DNS for ~40 s — the exact case failover exists for). A shorter timeout here makes every
    /// vantage answer `None` on a genuinely dead owner, and failover never happens.
    pub async fn probe_via(&self, via_addr: &str, target_name: &str) -> Option<bool> {
        let key = format!("{via_addr}|{target_name}");
        let fut = {
            let mut m = self.via_in_flight.lock().unwrap();
            if let Some(f) = m.get(&key) {
                f.clone()
            } else {
                use futures::FutureExt;
                let client = self.client.clone();
                let secret = self.secret.clone();
                let (v, t) = (via_addr.to_string(), target_name.to_string());
                let f = async move { probe_via_once(&client, &secret, &v, &t).await }.boxed().shared();
                m.insert(key.clone(), f.clone());
                f
            }
        };
        let out = fut.await;
        self.via_in_flight.lock().unwrap().remove(&key);
        out
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
            if !HOP_BY_HOP.contains(&k.as_str()) {
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
    match http_peer.rsplit_once(':') {
        Some((host, port)) => format!("{host}:{}", port.parse::<u16>().unwrap_or(8081) + 1),
        None => http_peer.to_string(),
    }
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
    if presented.is_empty() || presented != app.forwarder.secret {
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
    if !crate::auth::authorize(Some(owner.as_str()), &ro) {
        return refuse(reader, "access denied").await;
    }
    // Same rule as HTTP: re-check the nodes ranked above us from here (and vantages), forward up
    // if one answers unless out of hops — and at the hop limit, still refuse to serve what routing
    // says is not ours.
    let route = app.route(&format!("{ro}/{rn}")).await;
    if hops >= MAX_HOPS && !matches!(route, crate::peers::Route::Local) {
        return refuse(reader, "routing disagreement at hop limit; retry").await;
    }
    if hops < MAX_HOPS {
        match route {
            crate::peers::Route::Local => {}
            crate::peers::Route::Unavailable => {
                return refuse(reader, "no node may safely serve this repository; retry").await
            }
            crate::peers::Route::Peer(peer) => {
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
        // the owner may itself be routing (a few probes) before it answers.
        let mut status = String::new();
        // Grows with hops remaining: each downstream node may itself route (~9s) and wait on its own downstream, so the edge must outwait the whole chain.
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
