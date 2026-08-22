//! Read-only JSON views of a repo, on the peer listener only.
//!
//! Every handler makes the same three moves: `open` the repo read-only, parse the oid, then run
//! the (blocking) `browse` call on a blocking thread. The odb handle is opened inside that closure
//! rather than moved into it — `gix_odb::Handle` is not `Sync`.
//!
//! Split by concern: `images` (container-image routes), `repo` (refs/tree/blob/log/commit/
//! signature reads), `admin` (visibility/create/delete/protect — all owning-node writes), `merge`
//! (compare/merge/patch). Shared plumbing (`hidden`, `open_ro`, `odb_json`, `parse_oid`, `internal`,
//! `BLOB_CAP`) stays here because every submodule calls at least one of them.
mod admin;
mod images;
mod merge;
mod repo;

use super::{internal, open, Trusted};
use crate::store::Repo;
use crate::App;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use gix_hash::ObjectId;
use serde::Serialize;
use std::sync::Arc;

/// Largest blob returned inline; anything past this comes back `truncated`.
const BLOB_CAP: usize = 1024 * 1024;

/// The one answer a stranger may see. A private repo and a missing repo must be indistinguishable,
/// so 403/404/unknown-oid/unknown-path/bad-oid all land here.
/// Named apart from `api::not_found`, which builds the api tier's forwarded 404 with its own
/// headers: two same-named functions across a trust boundary is a trap for whoever edits one.
pub(super) fn hidden() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// `open`, with existence collapsed away. 401 passes through so a client knows to present a
/// token; every other refusal becomes a flat 404.
pub(super) async fn open_ro(
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
pub(super) async fn odb_json<T: Serialize + Send + 'static>(
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

pub(super) fn parse_oid(s: &str) -> Result<ObjectId, Response> {
    s.parse::<ObjectId>().map_err(|_| hidden())
}

use admin::{api_create, api_delete, api_protect, api_protections, api_visibility};
use images::{imagedelete, images, imagetagdelete, imagetags};
use merge::{api_compare, api_merge, api_patch};
use repo::{api_blob, api_commit, api_files, api_lastmod, api_log, api_refs, api_signature, api_tree, api_tree_root};

/// Every route here is peer-only. All but `images` are repo-scoped; `images` is owner-scoped — see
/// its own doc comment and `api_route` in `http.rs`.
///
/// A new one must also be added to `BROWSE_TAILS` in `http.rs`: the routing
/// middleware refuses an `/api/` path it does not recognise BEFORE the router
/// runs, so a route registered only here answers 404 and nothing explains why.
pub fn browse_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/{owner}/images", get(images))
        .route("/api/{owner}/{name}/imagetags", get(imagetags))
        .route("/api/{owner}/{name}/imagetagdelete", post(imagetagdelete))
        .route("/api/{owner}/{name}/imagedelete", post(imagedelete))
        .route("/api/{owner}/{name}/refs", get(api_refs))
        .route("/api/{owner}/{name}/tree/{oid}", get(api_tree_root))
        .route("/api/{owner}/{name}/tree/{oid}/{*path}", get(api_tree))
        .route("/api/{owner}/{name}/blob/{oid}/{*path}", get(api_blob))
        .route("/api/{owner}/{name}/log/{oid}", get(api_log))
        .route("/api/{owner}/{name}/commit/{oid}", get(api_commit))
        .route("/api/{owner}/{name}/files/{oid}", get(api_files))
        .route("/api/{owner}/{name}/lastmod/{oid}", get(api_lastmod))
        .route("/api/{owner}/{name}/compare", get(api_compare))
        .route("/api/{owner}/{name}/signature/{oid}", get(api_signature))
        .route(
            "/api/{owner}/{name}/merge",
            post(api_merge).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
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
        // A patch carries file contents, so this is the one write route with a real
        // body: 25 MiB, which is a generous edit and far below what a push is for.
        .route(
            "/api/{owner}/{name}/patch",
            post(api_patch).layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/delete",
            post(api_delete).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
        .route(
            "/api/{owner}/{name}/protect",
            get(api_protections).post(api_protect).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
}
