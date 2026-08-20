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
use serde::{Deserialize, Serialize};
use slatedb::object_store::ObjectStoreExt;
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

/// What a merge answers with: the commit the base now points at.
#[derive(Serialize)]
struct Merged {
    merged: String,
}

#[derive(Serialize)]
struct Ref {
    name: String,
    oid: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct ImageSummary {
    name: String,
    /// Object-store manifest count, NOT a tag count: `images` is owner-scoped and cannot route to
    /// any one image's database (tags and visibility both live there), so this reads only the
    /// shared object store — see the handler doc below.
    manifests: usize,
    /// When the newest manifest was written, epoch millis. `None` for an image whose manifests are
    /// gone but whose prefix remains — a push that uploaded blobs and never finished.
    updated_ms: Option<i64>,
}

/// `GET /api/{owner}/images` — the team's images, for the Container Images page.
///
/// Owner-scoped rather than repo-scoped, so it is the one browse route whose second segment is not
/// a repo name (see `api_route` in `http.rs`). It still routes: `images` is a `BROWSE_TAILS` entry,
/// but `repo_of` answers `None` for it and the request is served by whichever node received it.
/// That is only safe because this handler reads the shared object store ALONE — it must never call
/// `image_db`/`store.tags`/`store.image_is_public`, each of which opens a specific image's database
/// with no ownership check, fencing that image's legitimate owner if served on the wrong node. Tag
/// counts and visibility both live in that database, which is why `ImageSummary` carries neither.
async fn images(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path(owner): Path<String>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    let names = match crate::registry::routes::image_names(&app, &owner).await {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let mut out = vec![];
    for name in names {
        let (manifests, updated_ms) =
            crate::registry::store::manifest_stat(&app.store, &owner, &name)
                .await
                .unwrap_or((0, None));
        out.push(ImageSummary { name, manifests, updated_ms });
    }
    Json(out).into_response()
}

#[derive(Serialize)]
struct ImageTag {
    tag: String,
    digest: String,
    /// The manifest document's own size on disk — kilobytes, not the image's size.
    size: u64,
    /// What pulling this tag actually transfers: the config blob plus every layer, as the manifest
    /// itself declares them. Summed from the manifest rather than stored, because nothing writes an
    /// image-size field and a stored one could disagree with the layers that are really there.
    bytes: u64,
    /// When this manifest was written, epoch millis, from the object store's own mtime.
    pushed_ms: Option<i64>,
}

/// `GET /api/{owner}/{image}/imagetags` — the tag rows the image page needs. Shaped like every
/// other repo-scoped browse route (`{image}` fills the `{name}` slot), but it routes by the IMAGE
/// key (`registry::routing_key`, `img/{owner}/{name}`), not the repo key: `repo_of` in `http.rs`
/// special-cases the `imagetags` tail so this reaches the node that actually holds the image's
/// database, which may differ from whatever node owns a git repo of the same name.
async fn imagetags(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    let tags = match app.store.tags(&owner, &name).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let mut out = vec![];
    for tag in tags {
        let Some(d) = app.store.tag(&owner, &name, &tag).await.unwrap_or(None) else {
            continue;
        };
        // The manifest's own bytes, not a maintained size field: nothing writes one, and asking
        // the object store directly can never disagree with what was actually pushed.
        let path = crate::registry::store::manifest_path(&owner, &name, &d);
        let meta = app.store.os.head(&path).await.ok();
        let size = meta.as_ref().map(|m| m.size).unwrap_or(0);
        let pushed_ms = meta.as_ref().map(|m| m.last_modified.timestamp_millis());
        // Reading the manifest to ADD UP its declared sizes — never to re-emit it. The digest is
        // over the exact bytes, so nothing here may write a manifest back.
        let bytes = match app.store.os.get(&path).await {
            Ok(r) => match r.bytes().await {
                Ok(b) => declared_size(&b),
                Err(_) => 0,
            },
            Err(_) => 0,
        };
        out.push(ImageTag { tag, digest: d.to_string(), size, bytes, pushed_ms });
    }
    Json(out).into_response()
}

/// What a pull of this manifest transfers: its config blob plus every layer.
///
/// An index (a multi-platform image) names other MANIFESTS rather than layers; its entries carry
/// their own `size`, so summing them gives the index's total across platforms. Anything
/// unrecognised sums to zero rather than guessing — a wrong number shown confidently is worse than
/// no number.
fn declared_size(bytes: &[u8]) -> u64 {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else { return 0 };
    let mut total = v.get("config").and_then(|c| c.get("size")).and_then(|s| s.as_u64()).unwrap_or(0);
    for key in ["layers", "manifests"] {
        if let Some(items) = v.get(key).and_then(|l| l.as_array()) {
            total += items.iter().filter_map(|l| l.get("size")?.as_u64()).sum::<u64>();
        }
    }
    total
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

/// Delete a repo ON THE NODE THAT OWNS IT — same routing reason as `create` and
/// `visibility`. Idempotent: a repo that is already gone is the end state the
/// caller asked for, so it answers 204 rather than 404, which lets the api tier
/// clean up an index row whose repo was removed some other way.
async fn api_delete(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return StatusCode::NO_CONTENT.into_response();
    }
    match app.store.delete_repo(&owner, &name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("delete-repo {owner}/{name}: {e}"); // ponytail: eprintln
            (StatusCode::INTERNAL_SERVER_ERROR, "could not delete the repository").into_response()
        }
    }
}

/// Branch protection rules. GET lists them; POST sets one; POST with `remove`
/// drops one. On the owning node because the rules live in the repo's own
/// database — the same database the push path reads them from.
async fn api_protect(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    let Some(pattern) = q.get("pattern").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "a branch pattern is required").into_response();
    };
    let result = if q.get("remove").is_some() {
        app.store.remove_protection(&owner, &name, pattern).await
    } else {
        app.store
            .set_protection(
                &owner,
                &name,
                &crate::refs::Protection {
                    pattern: pattern.to_string(),
                    // Absent means on: a rule that forbids nothing is a rule
                    // someone believes is protecting them.
                    no_force: q.get("no_force").map(|v| v != "0").unwrap_or(true),
                    no_delete: q.get("no_delete").map(|v| v != "0").unwrap_or(true),
                },
            )
            .await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("pattern") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            eprintln!("protect {owner}/{name}: {msg}"); // ponytail: eprintln
            (StatusCode::INTERNAL_SERVER_ERROR, "could not save the rule").into_response()
        }
    }
}

async fn api_protections(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    match app.store.protections(&owner, &name).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}

/// What a branch would bring to another, and whether it can be applied without a
/// merge commit. Both refs are SHORT branch names — a review is about branches,
/// and resolving them here means the answer follows a push rather than pinning to
/// whatever oid a client last saw.
async fn api_compare(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    let (base_ref, head_ref) = (format!("refs/heads/{base}"), format!("refs/heads/{head}"));
    let (base_oid, head_oid) = match (
        app.store.get_ref(&repo, &base_ref).await,
        app.store.get_ref(&repo, &head_ref).await,
    ) {
        (Ok(Some(b)), Ok(Some(h))) => (b, h),
        (Err(e), _) | (_, Err(e)) => return internal(e),
        // A branch that is not there is the caller's mistake to see, not a 500.
        _ => return (StatusCode::NOT_FOUND, "no such branch").into_response(),
    };
    let n = q.get("n").and_then(|v| v.parse::<usize>().ok()).unwrap_or(250).clamp(1, 1000);
    odb_json(repo, move |odb| crate::browse::compare(odb, base_oid, head_oid, n)).await
}

/// Apply a change by moving `base` to `head`.
///
/// Fast-forward only. A true merge means writing a new commit, and a new commit
/// means a three-way merge of two trees — real work that can conflict, which this
/// server cannot yet do. Moving a ref cannot conflict and cannot lose anything, so
/// it is the honest subset to ship first. Anything else is refused with the reason,
/// and the branch owner rebases.
///
/// It goes through `update_refs`, so BRANCH PROTECTION applies to a merge exactly
/// as it applies to a push — a protected base is not a back door.
async fn api_merge(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    let repo = match app.store.open_repo(&owner, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return hidden(),
        Err(e) => return internal(e),
    };
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    let (base_ref, head_ref) = (format!("refs/heads/{base}"), format!("refs/heads/{head}"));
    let (base_oid, head_oid) = match (
        app.store.get_ref(&repo, &base_ref).await,
        app.store.get_ref(&repo, &head_ref).await,
    ) {
        (Ok(Some(b)), Ok(Some(h))) => (b, h),
        (Err(e), _) | (_, Err(e)) => return internal(e),
        _ => return (StatusCode::NOT_FOUND, "no such branch").into_response(),
    };

    let odb = match repo.odb() {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    // Re-checked HERE rather than trusted from whatever the caller last read: the
    // branch may have moved since the page was rendered.
    if crate::browse::merge_base(&odb, base_oid, head_oid, 50_000) != Some(base_oid) {
        return (
            StatusCode::CONFLICT,
            "this branch is behind its base — rebase it and push again",
        )
            .into_response();
    }

    // Which shape to land it in. All three are safe HERE and only here: the base
    // is an ancestor of the head, so the content being landed is exactly the
    // head's tree and no three-way merge is possible or needed. On a diverged
    // branch these would each need a real merge, which is why that case is
    // refused above rather than guessed at.
    let strategy = q.get("strategy").map(String::as_str).unwrap_or("fast-forward");
    let new_tip = match strategy {
        // The ref simply moves; no new object.
        "fast-forward" | "rebase" => head_oid,
        "squash" | "merge" => {
            let mut buf = Vec::new();
            let head_commit = match gix_object::FindExt::find_commit(&odb, &head_oid, &mut buf) {
                Ok(c) => c,
                Err(e) => return internal(crate::err(e.to_string())),
            };
            let tree = head_commit.tree();
            let author = head_commit.author().ok();
            let (who, mail) = match &author {
                Some(a) => (a.name.to_string(), a.email.to_string()),
                None => ("kloudlite".to_string(), "noreply@kloudlite.io".to_string()),
            };
            // The commit time comes from the head commit, not the clock, so
            // merging the same branch twice produces the same id — which is what
            // makes a retried merge idempotent rather than duplicating history.
            let time = author.as_ref().and_then(|a| a.time().ok()).map(|t| t.seconds).unwrap_or(0);
            let parents = if strategy == "squash" {
                vec![base_oid]
            } else {
                vec![base_oid, head_oid]
            };
            let message = q
                .get("message")
                .cloned()
                .unwrap_or_else(|| format!("Merge {head} into {base}\n"));

            match crate::objects::write_commit(
                &app.store,
                &repo,
                crate::objects::NewCommit {
                    tree,
                    parents,
                    message,
                    author_name: who,
                    author_email: mail,
                    time,
                },
            )
            .await
            {
                Ok(oid) => oid,
                Err(e) => return internal(e),
            }
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "strategy must be fast-forward, squash, merge or rebase",
            )
                .into_response()
        }
    };

    let update = vec![crate::refs::RefUpdate {
        name: format!("refs/heads/{base}"),
        old: Some(base_oid),
        new: Some(new_tip),
    }];
    match app.store.update_refs(&repo, &update).await {
        Ok(r) => match r.into_iter().next().flatten() {
            None => Json(Merged { merged: new_tip.to_hex().to_string() }).into_response(),
            Some(reason) => (StatusCode::CONFLICT, reason).into_response(),
        },
        Err(e) => internal(e),
    }
}

/// One file's worth of a patch, as the api tier sends it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileChange {
    path: String,
    /// Base64: a file is arbitrary bytes and JSON carries text, so the bytes
    /// cannot go over as a string. Absent when `delete` is set.
    content_base64: Option<String>,
    /// `None` keeps the mode the file already has.
    executable: Option<bool>,
    #[serde(default)]
    delete: bool,
}

/// A patch: one commit, any number of files.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Patch {
    /// The branch the editor was reading.
    branch: String,
    /// The tip it was reading, if the caller knows it. The commit is refused if
    /// the branch has moved since — someone pushed while the editor was open,
    /// and landing on top of a tip we never saw would silently drop their work.
    expect: Option<String>,
    message: String,
    author_name: String,
    author_email: String,
    /// Commit onto a NEW branch of this name instead of moving `branch`. This is
    /// what "start a pull request from this edit" is: the base branch does not
    /// move at all, so it can be protected and the edit still lands.
    new_branch: Option<String>,
    changes: Vec<FileChange>,
}

#[derive(Serialize)]
struct Committed {
    commit: String,
    branch: String,
}

/// Apply a patch as one commit.
///
/// The whole patch lands or none of it does: the blobs and trees are staged and
/// written together, and the ref moves only once the commit is stored. And the
/// ref moves by compare-and-swap, so a push that arrives mid-edit loses the race
/// rather than being overwritten.
async fn api_patch(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Json(patch): Json<Patch>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    let repo = match app.store.open_repo(&owner, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return hidden(),
        Err(e) => return internal(e),
    };
    if patch.changes.is_empty() {
        return (StatusCode::BAD_REQUEST, "a commit needs at least one change").into_response();
    }
    if patch.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a commit needs a message").into_response();
    }

    let branch_ref = format!("refs/heads/{}", patch.branch);
    let tip = match app.store.get_ref(&repo, &branch_ref).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    // Read HERE rather than trusted from whatever the editor last saw.
    if let Some(expected) = &patch.expect {
        if tip.map(|t| t.to_hex().to_string()).as_deref() != Some(expected.as_str()) {
            return (
                StatusCode::CONFLICT,
                "this branch has moved since you started editing",
            )
                .into_response();
        }
    }
    let Some(tip) = tip else {
        return (StatusCode::NOT_FOUND, "no such branch").into_response();
    };

    let odb = match repo.odb() {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    let mut buf = Vec::new();
    let base_tree = match gix_object::FindExt::find_commit(&odb, &tip, &mut buf) {
        Ok(c) => c.tree(),
        Err(e) => return internal(crate::err(e.to_string())),
    };

    let mut changes = std::collections::BTreeMap::new();
    for c in patch.changes {
        let change = if c.delete {
            crate::objects::Change::Delete
        } else {
            use base64::Engine;
            let Some(b64) = c.content_base64.as_deref() else {
                return (StatusCode::BAD_REQUEST, format!("{}: no content", c.path)).into_response();
            };
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(content) => crate::objects::Change::Upsert { content, executable: c.executable },
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, format!("{}: content is not base64", c.path))
                        .into_response()
                }
            }
        };
        // Two changes to one path have no defined order, so the patch is refused
        // rather than one of them silently winning.
        if changes.insert(c.path.clone(), change).is_some() {
            return (StatusCode::BAD_REQUEST, format!("{} appears twice", c.path)).into_response();
        }
    }

    let mut staging = crate::objects::Staging::default();
    let tree = match crate::objects::apply_changes(&odb, Some(base_tree), &changes, &mut staging) {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    // Nothing actually changed: the same bytes were sent back. A commit here
    // would be an empty one, which is noise in the history rather than a record.
    if tree == base_tree {
        return (StatusCode::BAD_REQUEST, "this changes nothing").into_response();
    }

    // Blobs and trees FIRST: a commit is validated against what is stored, so it
    // cannot be written before the tree it points at.
    if let Err(e) = staging.write(&app.store, &repo).await {
        return internal(e);
    }
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let commit = match crate::objects::write_commit(
        &app.store,
        &repo,
        crate::objects::NewCommit {
            tree,
            parents: vec![tip],
            message: patch.message,
            author_name: patch.author_name,
            author_email: patch.author_email,
            time,
        },
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return internal(e),
    };

    // Onto a new branch, the update is a CREATE (`old: None`), so it cannot
    // overwrite a branch of that name that already exists.
    let (target, old) = match &patch.new_branch {
        Some(b) => (format!("refs/heads/{b}"), None),
        None => (branch_ref, Some(tip)),
    };
    let landed_on = patch.new_branch.clone().unwrap_or(patch.branch);
    match app
        .store
        .update_refs(&repo, &[crate::refs::RefUpdate { name: target, old, new: Some(commit) }])
        .await
    {
        Ok(r) => match r.into_iter().next().flatten() {
            None => Json(Committed { commit: commit.to_hex().to_string(), branch: landed_on })
                .into_response(),
            Some(reason) => (StatusCode::CONFLICT, reason).into_response(),
        },
        Err(e) => internal(e),
    }
}

#[derive(Serialize)]
struct SignatureOf {
    signature: String,
    /// Base64: the payload is raw object bytes, which JSON cannot carry.
    payload_base64: String,
    author_email: String,
}

/// A commit's signature and the bytes it covers.
///
/// The node can produce these but cannot judge them — it has no list of whose
/// keys are whose. Verification belongs to the api tier, which does.
async fn api_signature(
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
        crate::browse::signature_of(odb, oid).map(|s| {
            s.map(|s| {
                use base64::Engine;
                SignatureOf {
                    signature: s.signature,
                    payload_base64: base64::engine::general_purpose::STANDARD.encode(&s.payload),
                    author_email: s.author_email,
                }
            })
        })
    })
    .await
}

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
