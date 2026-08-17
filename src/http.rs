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
use base64::Engine;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

/// Cap on a single request body (compressed bytes on the wire). Axum enforces this in the
/// extractor, BEFORE the handler runs, so an unauthenticated client cannot make the server
/// buffer more than this. Override with RUSTIC_GIT_MAX_BODY (bytes).
fn max_body() -> usize {
    std::env::var("RUSTIC_GIT_MAX_BODY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024) // 2 GiB
}

/// Cap on the decompressed size of a gzipped request body — bounds the zlib-bomb amplification
/// on top of the wire-size limit. 8x the body cap.
fn max_decompressed() -> u64 {
    (max_body() as u64) * 8
}

/// Liveness/readiness probe.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    // A fenced repo is not a sick node: the balancer has moved that repo's writer elsewhere, and
    // the pool drops the stale handle on the next write. Only report the warm set, so an
    // orchestrator can see the node is up and how much it is holding.
    (StatusCode::OK, format!("ok ({} warm)", app.store.pool.warm_count())).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
        .layer(axum::extract::DefaultBodyLimit::max(max_body()))
        .with_state(app)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"rustic-git\"")],
        "auth required",
    )
        .into_response()
}

fn internal(e: crate::Error) -> Response {
    eprintln!("internal error: {e}"); // ponytail: eprintln; swap for a logger when one exists
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

async fn open(app: &App, headers: &HeaderMap, owner: &str, name: &str) -> Result<Repo, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|d| String::from_utf8(d).ok())
        .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
    let Some(token) = token else {
        return Err(unauthorized());
    };
    let auth_owner = app.store.owner_for_token(&token).await.map_err(internal)?;
    if auth_owner.is_none() {
        return Err(unauthorized());
    }
    if !crate::auth::authorize(auth_owner.as_deref(), owner) {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let (owner, name) =
        crate::protocol::parse_repo_path(&format!("{owner}/{name}")).unwrap_or_default();
    match app.store.open_repo(&owner, &name).await {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
        Err(e) => {
            eprintln!("open_repo {owner}/{name}: {e}"); // ponytail: eprintln; swap for a logger when one exists
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

async fn info_refs(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let service = q.get("service").cloned().unwrap_or_default();
    let repo = match open(&app, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("version=2"))
        .unwrap_or(false);
    let store = app.store.clone();
    let svc = service.clone();
    let res = tokio::task::spawn_blocking(move || -> crate::Result<Vec<u8>> {
        let mut out = Vec::new();
        match svc.as_str() {
            "git-upload-pack" => {
                if !v2 {
                    return Err(crate::err("protocol v2 required"));
                }
                upload::advertise(&mut out)?;
            }
            "git-receive-pack" => {
                crate::pktline::write_text(&mut out, "# service=git-receive-pack")?;
                crate::pktline::write_flush(&mut out)?;
                receive::advertise(&store, &repo, &mut out)?;
            }
            _ => return Err(crate::err("unknown service")),
        }
        Ok(out)
    })
    .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    match res {
        Ok(out) => (
            [
                (
                    header::CONTENT_TYPE,
                    format!("application/x-{service}-advertisement"),
                ),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
            out,
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ponytail: whole request/response buffered in memory; stream when repos get big
async fn upload_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let store = app.store.clone();
    let mut input = std::io::BufReader::new(body_reader(&headers, body));
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let res = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        upload::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
    })
    .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    respond("application/x-git-upload-pack-result", res)
}

async fn receive_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let repo = match open(&app, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let store = app.store.clone();
    let mut input = std::io::BufReader::new(body_reader(&headers, body));
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = Disconnect(flag.clone());
    let res = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(&store, &repo, &mut input, &mut out, &flag).map(|_| out)
    })
    .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => return internal(crate::err(e.to_string())),
    };
    respond("application/x-git-receive-pack-result", res)
}

fn respond(ct: &'static str, res: crate::Result<Vec<u8>>) -> Response {
    match res {
        Ok(out) => (
            [
                (header::CONTENT_TYPE, ct),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            out,
        )
            .into_response(),
        Err(e) => internal(e),
    }
}
