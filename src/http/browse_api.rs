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
    routing::{get, post},
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

/// Flip a repo's visibility ON THE NODE THAT OWNS IT. Everything else under `/api/` is a read;
/// this one changes live authorization, and that is exactly why it is here: `admin set-visibility`
/// used to open the repo's database as a second process while the owning node kept answering from
/// its own handle, so a repo could be private in the database and still authorized as public by the
/// node serving it (~4s observed). Routed like every other repo-scoped path, the write lands on the
/// same handle that serves the repo — one writer, one view.
///
/// Authorization is the peer secret alone, deliberately. The secret already grants a caller the
/// right to be told any private repo's contents (`trust_peer` + `Trusted`), so it is not a weaker
/// gate than the reads beside it; and `admin` is a superuser tool, so requiring `OWNER_HEADER` to
/// match the repo's owner would break the legitimate operator case it exists for. The route is on
/// the peer router only, and `route_inner` 404s every `/api/` path on the public listener.
async fn api_visibility(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let public = match q.get("visibility").map(String::as_str) {
        Some("public") => true,
        Some("private") => false,
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    // Asks the object store, not the pool: `set_public` goes through `db_for`, which CREATES a
    // database for whatever name it is handed.
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    // `set_public` already bumps the cache generation and, on failure, carries the retry
    // instruction in its message. Passed through verbatim so the operator sees it.
    match app.store.set_public(&owner, &name, public).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("set-visibility {owner}/{name}: {e}"); // ponytail: eprintln
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Create a repo ON THE NODE THAT OWNS IT, for the same reason `visibility` lives here: a second
/// process opening the repo's database while the owning node holds its own handle is two writers.
/// Routed by `api_route` like every other repo-scoped path, so the node that will serve the repo
/// is the node that creates it.
///
/// Authorization is the peer secret alone — identical to `visibility` beside it. Whether the
/// CALLER may create under this owner is the api tier's question, not this one's: only the api
/// server knows about users and teams, and this route is unreachable from the public listener.
async fn api_create(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let public = match q.get("visibility").map(String::as_str) {
        // Absent means private. A repo that defaults to public is a data leak waiting for the one
        // caller that forgets the parameter.
        None | Some("private") => false,
        Some("public") => true,
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    match app.store.create_repo(&owner, &name).await {
        Ok(()) => {}
        Err(e) => {
            let msg = e.to_string();
            // Already taken is the caller's answer to render, not our failure.
            if msg.contains("already exists") {
                return (StatusCode::CONFLICT, "repository already exists").into_response();
            }
            if msg.contains("invalid repo path") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("create-repo {owner}/{name}: {msg}"); // ponytail: eprintln
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not create repository").into_response();
        }
    }
    // Only after the repo exists, and only when asked for: `create_repo` leaves it private, so a
    // failure here leaves a private repo rather than a public one nobody meant to publish.
    if public {
        if let Err(e) = app.store.set_public(&owner, &name, true).await {
            eprintln!("create-repo {owner}/{name} visibility: {e}"); // ponytail: eprintln
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    StatusCode::CREATED.into_response()
}

/// Every file under a commit, in one answer. See `browse::files_at` — the caller
/// wants the shape of the repo, and a request per directory is what this replaces.
async fn api_files(
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
    let path = q.get("path").cloned().unwrap_or_default();
    // Clamped rather than refused, exactly as `log` clamps `n`.
    let cap = q.get("cap").and_then(|v| v.parse::<usize>().ok()).unwrap_or(5000).clamp(1, 20_000);
    odb_json(repo, move |odb| crate::browse::files_at(odb, oid, &path, cap)).await
}

#[derive(Serialize)]
struct LastChange {
    name: String,
    #[serde(flatten)]
    commit: crate::browse::Commit,
}

/// What last touched each entry of a directory. One walk of history for the whole
/// directory; see `browse::last_changes`.
async fn api_lastmod(
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
    let path = q.get("path").cloned().unwrap_or_default();
    let budget = q.get("budget").and_then(|v| v.parse::<usize>().ok()).unwrap_or(500).clamp(1, 2000);
    odb_json(repo, move |odb| {
        crate::browse::last_changes(odb, oid, &path, budget)
            .map(|v| v.into_iter().map(|(name, commit)| LastChange { name, commit }).collect::<Vec<_>>())
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
        .route("/api/{owner}/{name}/files/{oid}", get(api_files))
        .route("/api/{owner}/{name}/lastmod/{oid}", get(api_lastmod))
        // POST only, explicitly: the reads above are `get`, and a `visibility` route that also
        // answered GET would make a flip reachable by a plain browser fetch.
        .route(
            "/api/{owner}/{name}/visibility",
            // The handler never reads a body, but without a limit the route accepts an arbitrary
            // one — which a forwarding node streams to the owner before it is discarded.
            post(api_visibility).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
        .route(
            "/api/{owner}/{name}/create",
            post(api_create).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
}
