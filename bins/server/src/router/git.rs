use super::limits::{bad_request, client_err, fenced_elsewhere, internal, max_body, max_decompressed, ClientError};
use crate::protocol::{receive, upload};
use crate::store::Repo;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rustic_git_core::httpx::{basic_creds, unauthorized, Trusted};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

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

fn body_reader(headers: &HeaderMap, body: Bytes) -> Box<dyn Read + Send> {
    if headers
        .get(header::CONTENT_ENCODING)
        .map(|v| v == "gzip")
        .unwrap_or(false)
    {
        Box::new(flate2::read::GzDecoder::new(Cursor::new(body)).take(max_decompressed()))
    } else {
        Box::new(Cursor::new(body))
    }
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

// ponytail: whole request/response buffered in memory; stream when repos get big
async fn upload_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let store = app.store.clone();
    let hs = headers.clone();
    let run_protocol = move |repo: Repo, body: Bytes| {
        let (store, flag, hs) = (store.clone(), flag.clone(), hs.clone());
        async move {
            let mut input = std::io::BufReader::new(body_reader(&hs, body));
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                upload::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
            })
            .await
        }
    };
    respond_first(
        "application/x-git-upload-pack-result",
        &app,
        (&o, &n),
        run_protocol,
        body,
        repo,
    )
    .await
}

async fn receive_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &trusted, &headers, &owner, &name, false).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (o, n) = (repo.owner.clone(), repo.name.clone());
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let store = app.store.clone();
    let hs = headers.clone();
    let run_protocol = move |repo: Repo, body: Bytes| {
        let (store, flag, hs) = (store.clone(), flag.clone(), hs.clone());
        async move {
            let mut input = std::io::BufReader::new(body_reader(&hs, body));
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                receive::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
            })
            .await
        }
    };
    respond_first(
        "application/x-git-receive-pack-result",
        &app,
        (&o, &n),
        run_protocol,
        body,
        repo,
    )
    .await
}

type Joined = std::result::Result<crate::Result<Vec<u8>>, tokio::task::JoinError>;

/// Turn the first attempt into a response, and on a fence that routing says we may still own, run
/// it once more against a freshly opened handle. The body is `Bytes`, so that is a plain second
/// call.
async fn respond_first<F, Fut>(
    ct: &'static str,
    app: &App,
    (o, n): (&str, &str),
    run_protocol: F,
    body: Bytes,
    repo: Repo,
) -> Response
where
    F: Fn(Repo, Bytes) -> Fut,
    Fut: std::future::Future<Output = Joined>,
{
    let res = match run_protocol(repo, body.clone()).await {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    match res {
        Ok(out) => success(ct, out),
        // See App::on_fenced. If routing still says we own it, reopen and run the request again.
        Err(e) if crate::pool::is_fenced(&e) => match reopen_after_fence(app, o, n).await {
            None => fenced_elsewhere(),
            Some(repo) => match run_protocol(repo, body).await {
                Ok(Ok(out)) => success(ct, out),
                // a second fence is a real error, not retried again
                Ok(Err(e)) if is_client_fault(&e) => bad_request(&e),
                Ok(Err(e)) => internal(e),
                Err(e) => internal(crate::err(e.to_string())),
            },
        },
        Err(e) if is_client_fault(&e) => bad_request(&e),
        Err(e) => internal(e),
    }
}

/// Same distinction `info_refs` makes: an explicit `ClientError`, or a bare `io::Error` — the
/// only error kind pkt-line parsing and gzip decompression raise on malformed/truncated client
/// input in `protocol::{receive,upload}`. Everything else in the push/fetch path is our own
/// store/object code, which never returns `io::Error` directly, so this doesn't risk masking a
/// genuine server fault as a 400.
fn is_client_fault(e: &crate::Error) -> bool {
    e.downcast_ref::<ClientError>().is_some() || e.downcast_ref::<std::io::Error>().is_some()
}

fn success(ct: &'static str, out: Vec<u8>) -> Response {
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
        .layer(axum::extract::DefaultBodyLimit::max(max_body()))
}
