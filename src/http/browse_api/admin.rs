//! Owning-node repo administration: visibility, create/delete, and branch protection.
use super::{hidden, open_ro};
use super::super::{internal, Trusted};
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::collections::HashMap;
use std::sync::Arc;

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
pub(super) async fn api_visibility(
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
        Ok(()) => {
            write_marker(&app, &owner, &name, public).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            eprintln!("set-visibility {owner}/{name}: {e}"); // ponytail: eprintln
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Writes the listing-index marker for a repo. `description`/`created_by` are always empty here:
/// truth for those fields is still Mongo for this sub-project, filled in by the reconcile job and
/// the sub-2 cutover. A marker write failure is logged and swallowed — the marker is a view, never
/// the source of truth, so it must never fail the caller's actual create/flip/delete.
async fn write_marker(app: &App, owner: &str, name: &str, public: bool) {
    let m = crate::index::Marker {
        name: name.to_string(),
        public,
        created_by: String::new(),
        created_ms: crate::ownership::now_ms() as i64,
        description: String::new(),
        manifests: 0,
        updated_ms: 0,
    };
    if let Err(e) = crate::index::write(&app.store.os, crate::index::Kind::Repo, owner, &m).await {
        eprintln!("index write {owner}/{name}: {e}"); // ponytail: eprintln
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
pub(super) async fn api_create(
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
    write_marker(&app, &owner, &name, public).await;
    StatusCode::CREATED.into_response()
}

/// Delete a repo ON THE NODE THAT OWNS IT — same routing reason as `create` and
/// `visibility`. Idempotent: a repo that is already gone is the end state the
/// caller asked for, so it answers 204 rather than 404, which lets the api tier
/// clean up an index row whose repo was removed some other way.
pub(super) async fn api_delete(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return StatusCode::NO_CONTENT.into_response();
    }
    // Markers removed BEFORE storage: gone from listings first, so a crash mid-delete never
    // leaves a marker pointing at a repo that no longer exists.
    if let Err(e) = crate::index::remove(&app.store.os, crate::index::Kind::Repo, &owner, &name).await {
        eprintln!("index remove {owner}/{name}: {e}"); // ponytail: eprintln
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
pub(super) async fn api_protect(
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

pub(super) async fn api_protections(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    // A repo's protection rules are as private as the repo. Gate exactly like `api_compare`:
    // 404 for a caller who may not see it, 401 to prompt for a token.
    if let Err(r) = open_ro(&app, &trusted, &headers, &owner, &name).await {
        return r;
    }
    match app.store.protections(&owner, &name).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}
