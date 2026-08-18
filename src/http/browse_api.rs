//! Read-only JSON views of a repo, on the peer listener only.
//!
//! Every handler makes the same three moves: `open` the repo read-only, parse the oid, then run
//! the (blocking) `browse` call on a blocking thread. The odb handle is opened inside that closure
//! rather than moved into it — `gix_odb::Handle` is not `Sync`.
use super::{internal, open, Trusted};
use crate::store::Repo;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use gix_hash::ObjectId;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Largest blob returned inline; anything past this comes back `truncated`.
const BLOB_CAP: usize = 1024 * 1024;

/// The one answer a stranger may see. A private repo and a missing repo must be indistinguishable,
/// so 403/404/unknown-oid/unknown-path/bad-oid all land here.
/// Named apart from `api::not_found`, which builds the api tier's forwarded 404 with its own
/// headers: two same-named functions across a trust boundary is a trap for whoever edits one.
fn hidden() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// `open`, with existence collapsed away. 401 passes through so a client knows to present a
/// token; every other refusal becomes a flat 404.
async fn open_ro(
    app: &App,
    trusted: &Trusted,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<Repo, Response> {
    match open(app, trusted, headers, owner, name, true).await {
        Ok(r) => Ok(r),
        Err(r) if r.status() == StatusCode::UNAUTHORIZED => Err(r),
        // A 500 stays a 500: hiding a bug behind 404 is not the leak we are defending against.
        Err(r) if r.status() == StatusCode::INTERNAL_SERVER_ERROR => Err(r),
        Err(r) if r.status() == StatusCode::SERVICE_UNAVAILABLE => Err(r),
        Err(_) => Err(hidden()),
    }
}

/// Run a blocking `browse` call against the repo's odb. A lookup failure (unknown oid, unknown
/// path, wrong object kind) collapses to 404; a real read failure — a corrupt or unreadable pack —
/// is a 500, because a bug that hides behind a 404 is a bug nobody finds.
async fn odb_json<T: Serialize + Send + 'static>(
    repo: Repo,
    f: impl FnOnce(&gix_odb::Handle) -> crate::Result<T> + Send + 'static,
) -> Response {
    let done = tokio::task::spawn_blocking(move || repo.odb().map(|odb| f(&odb))).await;
    match done {
        Ok(Ok(Ok(v))) => Json(v).into_response(),
        // The browse call itself failed: the object or path is not there, as far as a client is
        // allowed to know.
        Ok(Ok(Err(e))) => {
            eprintln!("browse: {e}"); // ponytail: eprintln
            if crate::browse::is_not_found(&e) {
                hidden()
            } else {
                internal(e)
            }
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(crate::err(format!("browse task: {e}"))),
    }
}

fn parse_oid(s: &str) -> Result<ObjectId, Response> {
    s.parse::<ObjectId>().map_err(|_| hidden())
}

#[derive(Serialize)]
struct Ref {
    name: String,
    oid: String,
    kind: &'static str,
}

async fn api_refs(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let refs = match app.store.list_refs(&repo).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let out: Vec<Ref> = refs
        .into_iter()
        .map(|(name, oid)| Ref {
            kind: if name.starts_with("refs/tags/") { "tag" } else { "branch" },
            name,
            oid: oid.to_hex().to_string(),
        })
        .collect();
    Json(out).into_response()
}

async fn tree(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    owner: String,
    name: String,
    oid: String,
    path: String,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| crate::browse::tree_at(odb, oid, &path)).await
}

async fn api_tree_root(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Response {
    tree(app, trusted, headers, owner, name, oid, String::new()).await
}

async fn api_tree(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid, path)): Path<(String, String, String, String)>,
) -> Response {
    tree(app, trusted, headers, owner, name, oid, path).await
}

async fn api_blob(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid, path)): Path<(String, String, String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| crate::browse::blob_at(odb, oid, &path, BLOB_CAP)).await
}

async fn api_log(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // Clamped, not rejected: `n` is a page size, and a client asking for a million commits wants
    // the first page, not an error.
    let n = q
        .get("n")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    odb_json(repo, move |odb| crate::browse::log(odb, oid, n)).await
}

#[derive(Serialize)]
struct CommitDetail {
    #[serde(flatten)]
    commit: crate::browse::Commit,
    diff: String,
}

async fn api_commit(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| {
        crate::browse::commit(odb, oid).map(|(commit, diff)| CommitDetail { commit, diff })
    })
    .await
}

pub fn browse_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/{owner}/{name}/refs", get(api_refs))
        .route("/api/{owner}/{name}/tree/{oid}", get(api_tree_root))
        .route("/api/{owner}/{name}/tree/{oid}/{*path}", get(api_tree))
        .route("/api/{owner}/{name}/blob/{oid}/{*path}", get(api_blob))
        .route("/api/{owner}/{name}/log/{oid}", get(api_log))
        .route("/api/{owner}/{name}/commit/{oid}", get(api_commit))
}
