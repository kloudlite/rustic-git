use super::limits::{bad_request, client_err, fenced_elsewhere, internal, max_decompressed, ClientError};
use rustic_git_core::httpx::max_body;
use crate::protocol::{receive, upload};
use crate::store::Repo;
use crate::App;
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rustic_git_core::httpx::{basic_creds, unauthorized, Trusted};
use rustic_git_storage::store::Store;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, Cursor, Read, Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc::Receiver, Semaphore};
use tokio_util::io::{StreamReader, SyncIoBridge};

pub(crate) async fn open(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    read_only: bool,
) -> Result<Repo, Response> {
    // A peer already authenticated this client; its word is trusted because `trust_peer` has
    // checked the shared secret. The public listener always presents `Trusted(None)`.
    let auth_owner = match &trusted.0 {
        Some(o) => Some(o.clone()),
        None => {
            match basic_creds(headers) {
                Some((user, t)) => {
                    // The token is the secret, but the username must name the owner it belongs to
                    // (or be git's `x` placeholder): halves that disagree did not verify, and the
                    // answer is a refusal, never a silent fall-through to anonymous.
                    match app.store.owner_for_token(&t).await.map_err(internal)? {
                        Some(o) if crate::auth::user_names(&user, &o, true) => Some(o),
                        _ => return Err(unauthorized()),
                    }
                }
                // No credentials is not yet a failure: a public repo may still admit this caller.
                None => None,
            }
        }
    };
    // Parsed before the visibility check: the raw path segment still carries `.git`, and looking
    // that up would warm a second, bogus pool entry alongside the repo's real one.
    let Some((owner, name)) = crate::protocol::parse_repo_pair(owner, name) else {
        return Err((StatusCode::BAD_REQUEST, "invalid repository path").into_response());
    };
    // Gated on `repo_public`, which asks the object store rather than the pool first: opening a
    // database through `db_for` CREATES one for whatever name it is handed. Unguarded, an
    // anonymous request on the public listener could conjure and warm a repo per mistyped path.
    let public = app.store.repo_public(&owner, &name).await.unwrap_or(false);
    if !crate::auth::authorize(auth_owner.as_deref(), &owner, public && read_only) {
        // No credentials at all gets 401, not 404/403: it tells the client to present a token,
        // whereas a private repo denied to an authenticated stranger looks like FORBIDDEN.
        return Err(if auth_owner.is_none() {
            unauthorized()
        } else {
            StatusCode::FORBIDDEN.into_response()
        });
    }
    match app.open_repo_after_fence(&owner, &name).await {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
        // Routing said another node owns it (or it fenced again): 503 so the client retries
        // against the owner.
        Err(e) if crate::pool::is_fenced(&e) => Err(fenced_elsewhere()),
        Err(e) => {
            tracing::error!(owner = %owner, repo = %name, error = %e, "open_repo");
            // We were routed here, so the map names us — and we have just proved we cannot serve.
            // Holding the lease anyway leaves the repo with an owner that cannot open it until the
            // TTL lapses; a forced claim makes that worse, because it fenced a peer to get here.
            // Give the lease back now, so the next request claims fresh instead of waiting.
            // Best-effort: a release that fails only means the TTL does the same job later.
            //
            // CLOSE FIRST. `open_repo` warms the database (`repo_exists` opens it) before the
            // step that failed, and `release` requires the handle already closed: a release with
            // the handle warm lets the next claimant open the database while this node's handle
            // is still live — two writers, until the fence lands and ownership flaps back.
            app.store.pool.evict(&owner, &name).await;
            let repo = format!("{owner}/{name}");
            if let Err(e) = app.release(&repo).await {
                tracing::warn!(repo = %repo, error = %e, "releasing after a failed open");
            }
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}

/// Signals the (blocking) protocol worker that the client is gone. Axum drops the handler future
/// when the connection closes, so dropping this guard is our disconnect notification — without it
/// an abandoned clone would keep building its pack to completion on a blocking thread.
struct Disconnect(Arc<std::sync::atomic::AtomicBool>);
impl Drop for Disconnect {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn body_reader(headers: &HeaderMap, raw: Box<dyn Read + Send>) -> Box<dyn Read + Send> {
    if headers
        .get(header::CONTENT_ENCODING)
        .map(|v| v == "gzip")
        .unwrap_or(false)
    {
        Box::new(flate2::read::GzDecoder::new(raw).take(max_decompressed()))
    } else {
        raw
    }
}

/// Cap on an upload-pack negotiation body. A `want`/`have` pkt-line is about 50 bytes, so this is
/// over 150 000 of them — past any real negotiation, and small enough that the concurrent-request
/// count that OOMs the pod is unreachable. NOT `max_body`: this body is BUFFERED, and an OOM here
/// moves repo ownership on an attacker's schedule.
const MAX_NEGOTIATION: usize = 8 * 1024 * 1024;

/// Read the whole body only AFTER `open()` has authenticated the caller. `Bytes` as an extractor
/// runs before the handler, so an anonymous client could make the pod buffer the whole cap and,
/// with a few of those in flight, OOM it. The `DefaultBodyLimit` layer only governs extractors, so
/// the cap is applied here by hand. Upload-pack only: its request is the negotiation, kilobytes —
/// so the cap is `MAX_NEGOTIATION`, not the 2 GiB `max_body` that governs receive-pack's STREAMED
/// body (`live_body`), which never sits in memory.
async fn read_body(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_NEGOTIATION)
        .await
        .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response())
}

/// The request body as a blocking `Read` the indexer pulls from directly, so a push never sits
/// in memory: `gix_pack::Bundle::write_to_directory` streams from any `BufRead`. `max_body`
/// applies to the bytes on the wire, the same cap `read_body` enforces, but it surfaces as a
/// read error inside the protocol — git shows it as the push's report line — rather than a 413,
/// because it is only known once the indexer has consumed that much.
fn live_body(body: Body) -> Box<dyn Read + Send> {
    use futures::StreamExt;
    let cap = max_body();
    let mut seen = 0usize;
    let stream = body.into_data_stream().map(move |c| {
        let c = c.map_err(std::io::Error::other)?;
        seen = seen.saturating_add(c.len());
        if seen > cap {
            return Err(std::io::Error::other("request body too large"));
        }
        Ok(c)
    });
    Box::new(SyncIoBridge::new(StreamReader::new(stream)))
}

/// Copies what it reads into `spool`, so the request can be replayed. Only the fence retry
/// (`respond`) ever reads it back, and the pack has already been read in full by the time a DB
/// write can observe a fence — a retry sees the whole request.
// ponytail: the spool doubles the push's disk write (the indexer writes the pack too) purely to
// keep the in-flight fence retry; if disk throughput ever matters, answer such a fence with 503
// and let git re-push instead.
struct Tee {
    inner: Box<dyn Read + Send>,
    spool: File,
}

impl Read for Tee {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.spool.write_all(&buf[..n])?;
        // Counted as they arrive, not from Content-Length: a push is chunked, and a client that
        // dies mid-pack still cost these bytes.
        metrics::counter!("git_pack_bytes_in_total", "op" => "receive").increment(n as u64);
        Ok(n)
    }
}

/// Bytes held back before the response starts going out. Below this the whole reply is still in
/// hand, so a fence can be retried and an error can still change the status line; the protocol
/// only touches the database before it writes anything, so a fence past this point does not
/// happen in practice. It is also the chunk size on the wire: `BandWriter` writes one pkt-line
/// at a time, and a channel send per pkt-line would cost more than the copy it saves.
const SPILL: usize = 64 * 1024;

/// The protocol's output on HTTP: a `Write` on the blocking side that becomes the response body
/// once `SPILL` bytes have accumulated. Bounded by the channel — a client that stops reading
/// stalls the pack build instead of growing the pod.
struct Streamed {
    buf: Vec<u8>,
    tx: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
    spilled: bool,
}

impl Streamed {
    fn send(&mut self) -> std::io::Result<()> {
        self.spilled = true;
        let chunk = Bytes::from(std::mem::take(&mut self.buf));
        // The receiver is the response body: gone means the client hung up, and a write error
        // is how the pack writer learns to stop.
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }
}

impl Write for Streamed {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        if self.buf.len() >= SPILL {
            self.send()?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.spilled && !self.buf.is_empty() {
            self.send()?;
        }
        Ok(())
    }
}

type Serve = fn(&Store, &Repo, &mut dyn BufRead, &mut dyn Write, &AtomicBool) -> crate::Result<()>;
type Input = std::io::Result<Box<dyn Read + Send>>;

enum Attempt {
    /// The reply spilled: its status is 200 and the rest is on the wire as it is produced.
    Streaming(Body),
    /// The reply finished (or failed) while still held back, so the caller decides the status.
    Done(crate::Result<Vec<u8>>),
}

async fn attempt(
    store: Arc<Store>,
    repo: Repo,
    serve: Serve,
    input: Input,
    headers: &HeaderMap,
    flag: Arc<AtomicBool>,
    guard: &mut Option<Disconnect>,
) -> Attempt {
    let input = match input {
        Ok(i) => i,
        Err(e) => return Attempt::Done(Err(e.into())),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut input = std::io::BufReader::new(body_reader(headers, input));
    let mut join = tokio::task::spawn_blocking(move || {
        let mut out = Streamed { buf: Vec::new(), tx, spilled: false };
        let res = serve(&store, &repo, &mut input, &mut out, &flag);
        if !out.spilled {
            return res.map(|()| Some(out.buf));
        }
        match res {
            Ok(()) => {
                let _ = out.flush();
            }
            // Past the status line the only report left is to break the body: hyper aborts the
            // chunked stream and git sees a truncated pack, not a clean one.
            Err(e) => {
                let _ = out.tx.blocking_send(Err(std::io::Error::other(e.to_string())));
            }
        }
        Ok(None)
    });
    let streaming = |first: Option<std::io::Result<Bytes>>,
                     rx: Receiver<std::io::Result<Bytes>>,
                     guard: &mut Option<Disconnect>| {
        // The guard rides with the body: the handler future ends when the response is returned,
        // and a guard dropped there would read as a disconnect at the first byte.
        let guard = guard.take();
        let rest = futures::stream::unfold((rx, guard), |(mut rx, g)| async move {
            let c = rx.recv().await?;
            Some((c, (rx, g)))
        });
        let head = futures::stream::iter(first);
        Attempt::Streaming(Body::from_stream(futures::StreamExt::chain(head, rest)))
    };
    let joined = tokio::select! {
        c = rx.recv() => match c {
            Some(c) => return streaming(Some(c), rx, guard),
            None => join.await,
        },
        r = &mut join => r,
    };
    match joined {
        Ok(Ok(Some(buf))) => Attempt::Done(Ok(buf)),
        Ok(Ok(None)) => streaming(None, rx, guard),
        Ok(Err(e)) => Attempt::Done(Err(e)),
        Err(e) => Attempt::Done(Err(crate::err(e.to_string()))),
    }
}

/// Pushes in flight on this pod at once. The pack no longer sits in memory, but each push still
/// indexes it on every core and spools it to the cache disk, so the count is what keeps a burst
/// from starving the repos already served here. Upload-pack is not gated: its request bodies
/// are small.
// ponytail: whole-pod counter, not per repo or per owner.
const RUSTIC_GIT_MAX_CONCURRENT_RECEIVE: usize = 2;

fn receive_permits() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(RUSTIC_GIT_MAX_CONCURRENT_RECEIVE))
}

fn too_many_pushes() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        "too many pushes in flight; retry",
    )
        .into_response()
}

/// The repo to run the second attempt against, after a fence that routing says we can still own.
/// `None` means the caller should answer 503.
async fn reopen_after_fence(app: &App, owner: &str, name: &str) -> Option<Repo> {
    if !app.on_fenced(owner, name).await {
        return None;
    }
    app.store.open_repo(owner, name).await.ok().flatten()
}

async fn info_refs(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
) -> Response {
    let service = q.get("service").cloned().unwrap_or_default();
    let repo = match open(&app, &trusted, &headers, &owner, &name, service == "git-upload-pack").await {
        Ok(r) => r,
        Err(r) => return r,
    };
    // NOT the raw Path `owner`/`name`: those still carry the `.git` suffix (every real URL has
    // it), which would name a database that does not exist.
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("version=2"))
        .unwrap_or(false);
    let store = app.store.clone();
    let svc = service.clone();
    let run_protocol = move |repo: Repo| {
        let (store, svc) = (store.clone(), svc.clone());
        async move {
            tokio::task::spawn_blocking(move || -> crate::Result<Vec<u8>> {
                let mut out = Vec::new();
                match svc.as_str() {
                    "git-upload-pack" => {
                        if !v2 {
                            return Err(client_err(
                                "this server requires git protocol v2 for git-upload-pack.\n\
                                 git 2.26+ uses protocol v2 by default; older clients can opt in \
                                 with:\n\n  git -c protocol.version=2 <command>\n",
                            ));
                        }
                        upload::advertise(&mut out)?;
                    }
                    "git-receive-pack" => {
                        crate::pktline::write_text(&mut out, "# service=git-receive-pack")?;
                        crate::pktline::write_flush(&mut out)?;
                        receive::advertise(&store, &repo, &mut out)?;
                    }
                    _ => return Err(client_err(format!("unknown service: {svc}"))),
                }
                Ok(out)
            })
            .await
        }
    };
    let success = |out: Vec<u8>| {
        (
            [
                (
                    header::CONTENT_TYPE,
                    format!("application/x-{service}-advertisement"),
                ),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
            out,
        )
            .into_response()
    };
    let res = match run_protocol(repo).await {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    match res {
        Ok(out) => success(out),
        // See App::on_fenced. If routing still says we own it, reopen and run the request again.
        Err(e) if crate::pool::is_fenced(&e) => match reopen_after_fence(&app, &o, &n).await {
            None => fenced_elsewhere(),
            Some(repo) => match run_protocol(repo).await {
                Ok(Ok(out)) => success(out),
                // a second fence is a real error, not retried again
                Ok(Err(e)) if e.downcast_ref::<ClientError>().is_some() => bad_request(&e),
                Ok(Err(e)) => internal(e),
                Err(e) => internal(crate::err(e.to_string())),
            },
        },
        Err(e) if e.downcast_ref::<ClientError>().is_some() => bad_request(&e),
        Err(e) => internal(e),
    }
}

async fn upload_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let body = match read_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    metrics::counter!("git_pack_requests_total", "op" => "upload").increment(1);
    metrics::counter!("git_pack_bytes_in_total", "op" => "upload").increment(body.len() as u64);
    let input = move || -> Input { Ok(Box::new(Cursor::new(body.clone()))) };
    respond("application/x-git-upload-pack-result", &app, repo, upload::serve, input, &headers).await
}

async fn receive_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, false).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    // A short wait absorbs a burst of small pushes; anything longer and git is better off
    // failing fast and retrying than sitting on an open connection we cannot serve.
    let _permit = match tokio::time::timeout(Duration::from_secs(2), receive_permits().acquire()).await {
        Ok(Ok(p)) => p,
        _ => return too_many_pushes(),
    };
    // Unlinked at creation: a pod killed mid-push leaves nothing behind. Under the pack dir
    // because that is the mount the indexer already writes to — the root is read-only.
    let spool = match tempfile::tempfile_in(&repo.pack_dir) {
        Ok(f) => f,
        Err(e) => return internal(e.into()),
    };
    metrics::counter!("git_pack_requests_total", "op" => "receive").increment(1);
    let mut live = Some(live_body(body));
    let input = move || -> Input {
        match live.take() {
            Some(inner) => Ok(Box::new(Tee { inner, spool: spool.try_clone()? })),
            None => {
                // `try_clone` shares the offset the tee left at the end; the rewind is what
                // makes this a replay.
                let mut f = spool.try_clone()?;
                f.seek(SeekFrom::Start(0))?;
                Ok(Box::new(f))
            }
        }
    };
    respond("application/x-git-receive-pack-result", &app, repo, receive::serve, input, &headers).await
}

/// Run the protocol; on a fence that routing says we may still own, run it once more against a
/// freshly opened handle. `input` is asked for a reader per attempt, so the retry can replay
/// what the first one consumed.
async fn respond(
    ct: &'static str,
    app: &App,
    repo: Repo,
    serve: Serve,
    mut input: impl FnMut() -> Input,
    headers: &HeaderMap,
) -> Response {
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let flag = Arc::new(AtomicBool::new(false));
    let mut guard = Some(Disconnect(flag.clone()));
    let store = app.store.clone();
    let first = attempt(store.clone(), repo, serve, input(), headers, flag.clone(), &mut guard).await;
    let res = match first {
        Attempt::Streaming(body) => return success(ct, body),
        Attempt::Done(r) => r,
    };
    match res {
        Ok(out) => success(ct, Body::from(out)),
        // See App::on_fenced. If routing still says we own it, reopen and run the request again.
        Err(e) if crate::pool::is_fenced(&e) => match reopen_after_fence(app, &o, &n).await {
            None => fenced_elsewhere(),
            Some(repo) => match attempt(store, repo, serve, input(), headers, flag, &mut guard).await {
                Attempt::Streaming(body) => success(ct, body),
                Attempt::Done(Ok(out)) => success(ct, Body::from(out)),
                // a second fence is a real error, not retried again
                Attempt::Done(Err(e)) if is_client_fault(&e) => bad_request(&e),
                Attempt::Done(Err(e)) => internal(e),
            },
        },
        Err(e) if is_client_fault(&e) => bad_request(&e),
        Err(e) => internal(e),
    }
}

/// Same distinction `info_refs` makes: an explicit `ClientError`, or an `io::Error` of the kinds
/// malformed/truncated client input produces — `Other` (pkt-line's own `io::Error::other`),
/// `UnexpectedEof`, and `InvalidData`/`InvalidInput` (gzip). Matched by KIND, not by type: the
/// push path also writes packs to local disk, and the OS reports a full or read-only disk as an
/// `io::Error` too (`StorageFull`, `ReadOnlyFilesystem`, `PermissionDenied`, `Uncategorized`…),
/// which used to come back as a 400 with the OS message in it.
///
/// `Other` is the one kind whose MESSAGE is echoed to the client in the 400 body, so every
/// producer of one on these paths must be a literal: pkt-line's own `io::Error::other`, and
/// `live_body`'s "request body too large". A future `Other` built with `format!` would put an
/// internal string on the wire — `the_only_other_kind_errors_answered_400_are_the_two_literals`
/// is what refuses one.
fn is_client_fault(e: &crate::Error) -> bool {
    use std::io::ErrorKind::*;
    e.downcast_ref::<ClientError>().is_some()
        || e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| matches!(e.kind(), Other | UnexpectedEof | InvalidData | InvalidInput))
}

fn success(ct: &'static str, out: Body) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        out,
    )
        .into_response()
}

pub(crate) fn git_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// A full disk under `create_dir_all` is ours to answer for (500), a bad pkt-line is theirs.
    #[test]
    fn os_io_errors_are_server_faults_and_protocol_ones_are_the_clients() {
        let fault = |e: Error| is_client_fault(&(Box::new(e) as crate::Error));
        assert!(fault(Error::other("bad pkt len")));
        assert!(fault(Error::new(ErrorKind::UnexpectedEof, "truncated")));
        assert!(fault(Error::new(ErrorKind::InvalidInput, "corrupt deflate stream")));
        assert!(!fault(Error::new(ErrorKind::StorageFull, "No space left on device")));
        assert!(!fault(Error::new(ErrorKind::ReadOnlyFilesystem, "Read-only file system")));
        assert!(!fault(Error::new(ErrorKind::PermissionDenied, "Permission denied")));
        assert!(!fault(Error::from_raw_os_error(libc_eio())));
        assert!(is_client_fault(&client_err("no such ref")));
        assert!(!is_client_fault(&crate::err("store: timeout")));
    }

    /// `Other` is answered 400 WITH ITS MESSAGE, so every producer of one on these paths must be
    /// a literal the client may read. Today that is pkt-line's own `io::Error::other` and
    /// `live_body`'s cap. This pins the contract: a new `Other` carrying an internal detail (a
    /// store URL, a peer address, a secret) must fail here rather than ship.
    #[test]
    fn the_only_other_kind_errors_answered_400_are_the_two_literals() {
        let fault = |e: Error| is_client_fault(&(Box::new(e) as crate::Error));
        // The two producers, by their exact strings.
        assert!(fault(Error::other("request body too large")));
        assert!(fault(Error::other("bad pkt len")));
        // The one `Other` the response path itself makes is a broken pipe, which is not `Other`
        // and so never reaches a client as text.
        let pipe = Error::new(ErrorKind::BrokenPipe, "client went away");
        assert!(!fault(pipe));
        // A source-level guard: nothing under the git router may construct an `io::Error::other`
        // whose message is built from a peer address or a store URL.
        let src = include_str!("git.rs");
        for line in src.lines().filter(|l| l.contains("Error::other(")) {
            assert!(
                !line.contains("format!") || line.contains("too large"),
                "an `Other` with a formatted message is echoed to the client verbatim: {line}"
            );
        }
    }

    /// EIO has no `ErrorKind` of its own, so it lands in `Uncategorized` — the kind every OS
    /// error the table does not know gets, and the one a whitelist must never answer 400 to.
    fn libc_eio() -> i32 {
        5
    }

    /// Below `SPILL` the reply is still ours to retry or fail; at `SPILL` it is on the wire.
    #[test]
    fn output_is_held_back_until_spill() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut out = Streamed { buf: Vec::new(), tx, spilled: false };
        out.write_all(&[1; SPILL - 1]).unwrap();
        assert!(!out.spilled && rx.try_recv().is_err());
        out.write_all(&[2]).unwrap();
        assert!(out.spilled && out.buf.is_empty());
        assert_eq!(rx.try_recv().unwrap().unwrap().len(), SPILL);
        // With the body gone, the next chunk is a write error — how the pack build stops.
        drop(rx);
        out.write_all(&[3; SPILL]).unwrap_err();
    }

    /// The spool is the request as the first attempt saw it, from the start.
    #[test]
    fn a_tee_replays_what_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let spool = tempfile::tempfile_in(dir.path()).unwrap();
        let mut tee = Tee { inner: Box::new(Cursor::new(b"abcdef".to_vec())), spool: spool.try_clone().unwrap() };
        let mut first = Vec::new();
        tee.read_to_end(&mut first).unwrap();
        let mut again = spool.try_clone().unwrap();
        again.seek(SeekFrom::Start(0)).unwrap();
        let mut replay = Vec::new();
        again.read_to_end(&mut replay).unwrap();
        assert_eq!(first, replay);
        assert_eq!(replay, b"abcdef");
    }
}
