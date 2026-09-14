//! A byte-for-byte TCP forwarder that stands in for an intercepted environment service.
//!
//! When a workspace takes over an environment's service, something in the ENVIRONMENT's namespace
//! has to hold the addresses callers dial: a Kubernetes selector can never leave its own namespace,
//! and an EndpointSlice naming a pod in another one is the kind of hand-written object that rots
//! the moment the pod moves. So a pod of this binary runs beside the service it replaces, keeps
//! the ordinary selector-backed Service pointing at itself, and forwards every byte to the
//! workspace's own Service. Every namespace that can reach the real service reaches this the same
//! way, with no second address to learn.
//!
//! What it must NEVER do: parse a protocol, log a payload (accept, close and error carry peer,
//! byte counts and port only), or resolve the target once at boot — a workspace recreate that
//! recreates the target Service must not need this pod restarted.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// No bytes in either direction for this long closes the connection.
///
/// Workspace pods restart often; without a deadline every restart leaks one half-open socket per
/// port until the proxy itself dies.
///
/// ponytail: fixed 600s idle; a ClusterSettings `Mark::Live` field is the upgrade path if a
/// long-poll is ever reported cut.
pub const IDLE_SECS: u64 = 600;

/// Default ceiling on in-flight connections. A workspace dev server is not a fleet service, and an
/// unbounded accept loop turns one loop in a caller into an OOM in the environment's namespace.
pub const DEFAULT_MAX_CONNS: usize = 512;

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The DNS name of the workspace-side target Service. An FQDN, because this pod's resolv.conf
    /// is the ENVIRONMENT namespace's and its search path does not contain the workspace's.
    pub target: String,
    /// `(listen_port, target_port)`, one per declared service port. The remap lives here.
    pub forwards: Vec<(u16, u16)>,
    pub max_conns: usize,
}

pub fn parse(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut target = None;
    let mut forwards = Vec::new();
    let mut max_conns = DEFAULT_MAX_CONNS;
    let mut it = argv;
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--target" => target = Some(val()?),
            "--max-conns" => {
                max_conns = val()?
                    .parse()
                    .map_err(|_| "--max-conns is not a number".to_string())?
            }
            "--forward" => {
                let v = val()?;
                let (l, t) = v
                    .split_once(':')
                    .ok_or_else(|| format!("--forward {v} is not listen:target"))?;
                let l: u16 = l
                    .parse()
                    .map_err(|_| format!("--forward {v}: {l} is not a port"))?;
                let t: u16 = t
                    .parse()
                    .map_err(|_| format!("--forward {v}: {t} is not a port"))?;
                if l == 0 || t == 0 {
                    return Err(format!("--forward {v}: 0 is not a port"));
                }
                forwards.push((l, t));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let target = target.ok_or("--target is required")?;
    if forwards.is_empty() {
        return Err("at least one --forward is required".into());
    }
    Ok(Args {
        target,
        forwards,
        max_conns,
    })
}

/// Bind EVERY listener before serving any.
///
/// A half-bound proxy that answers on 8080 and refuses 9229 is worse than a pod that will not
/// start: the Service would go Ready with half its ports dead, and the person would be debugging
/// their own app. A bind failure is fatal, deliberately.
pub async fn bind_all(args: &Args) -> Result<Vec<(TcpListener, u16)>, String> {
    let mut out = Vec::new();
    for (listen, to) in &args.forwards {
        let l = TcpListener::bind(("0.0.0.0", *listen))
            .await
            .map_err(|e| format!("bind 0.0.0.0:{listen}: {e}"))?;
        out.push((l, *to));
    }
    Ok(out)
}

pub async fn serve(args: Args) -> Result<(), String> {
    let listeners = bind_all(&args).await?;
    let limit = Arc::new(Semaphore::new(args.max_conns));
    let target = Arc::new(args.target);
    let mut tasks = Vec::new();
    for (l, to) in listeners {
        tasks.push(tokio::spawn(accept_loop(
            l,
            to,
            target.clone(),
            limit.clone(),
        )));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

pub async fn accept_loop(l: TcpListener, to: u16, target: Arc<String>, limit: Arc<Semaphore>) {
    loop {
        // The permit is taken BEFORE the accept, so over the bound the loop waits in the kernel's
        // backlog rather than spawning a task per pending connection.
        let Ok(permit) = limit.clone().acquire_owned().await else {
            return;
        };
        let (sock, peer) = match l.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, port = to, "proxy.accept_failed");
                continue;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Resolved PER CONNECTION, never once at boot: the target Service's ClusterIP is
            // stable, but a workspace recreate that recreates the Service must not need this pod
            // restarted to be reachable.
            match TcpStream::connect((target.as_str(), to)).await {
                Ok(up) => match pump(sock, up).await {
                    Ok((up_bytes, down_bytes)) => {
                        tracing::debug!(%peer, port = to, up_bytes, down_bytes, "proxy.closed")
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, %peer, port = to, "proxy.closed_with_error")
                    }
                },
                Err(e) => tracing::warn!(error = %e, %peer, port = to, "proxy.dial_failed"),
            }
        });
    }
}

/// The whole data path: copy both directions, half-close each as its source ends, give up after
/// `IDLE_SECS` with no bytes at all.
///
/// `copy_bidirectional` shuts each direction down as its source ends, which is what a client that
/// half-closes to signal end-of-request (plain HTTP/1.0, some RPC framings) needs; without it such
/// a client hangs forever. NOTHING here looks at the bytes.
pub async fn pump(mut client: TcpStream, mut upstream: TcpStream) -> std::io::Result<(u64, u64)> {
    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    // ponytail: this is a whole-connection deadline, not an idle one — a legitimate connection
    // open past IDLE_SECS is cut. A `Instant` last-byte tracker wrapped around the two halves is
    // the upgrade path; nothing in an environment holds a connection that long today.
    match tokio::time::timeout(Duration::from_secs(IDLE_SECS), copy).await {
        Ok(r) => r,
        Err(_) => {
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))
        }
    }
}
