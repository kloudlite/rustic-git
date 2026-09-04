//! Owning-node repo administration: visibility, create/delete, and branch protection.
use super::{hidden, open_ro};
use crate::router::internal;
use kloudlite_git_core::httpx::Trusted;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use slatedb::object_store::ObjectStoreExt;
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
    //
    // Serialized per {owner}/{name} so two racing flips cannot interleave `index::write`'s
    // delete-then-put (spec §6.5) — same guard `set_image_visibility` takes for images.
    let lock = app.store.keyed_lock(&crate::index::lock_key(crate::index::Kind::Repo, &owner, &name));
    let _guard = lock.lock().await;
    // Remove-permissive-first (spec §6.2) applies to the whole flip: on a private flip, delete
    // the PUBLIC marker before the DB row changes, so a crash between here and `write_marker`
    // can never leave a stale public marker over what the DB already calls private.
    if !public {
        let public_path = crate::index::path(true, crate::index::Kind::Repo, &owner, &name);
        if let Err(e) = crate::index::ignore_not_found(app.store.os.delete(&public_path).await) {
            tracing::warn!(owner = %owner, repo = %name, error = %e, "index pre-delete");
        }
    }
    match app.store.set_public(&owner, &name, public).await {
        Ok(()) => {
            write_marker(&app, &owner, &name, public, None).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            // The flag is written; only the cache bump can have failed. The operator's next step
            // is fixed text — the backend's own words stay in the log.
            tracing::error!(owner = %owner, repo = %name, error = %e, "set-visibility");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("visibility changed but cached answers may be stale; retry with `admin purge-cache {owner}/{name}`"),
            )
                .into_response()
        }
    }
}

/// Writes the listing-index marker for a repo. `meta` is `Some` only on create, where the caller
/// supplied the description and author; every other path preserves whatever the existing marker
/// already carries, because listings now read those fields from HERE — a flip that blanked them
/// would empty the description out of every listing. A marker write failure is logged and
/// swallowed — the marker is a view, never the source of truth, so it must never fail the
/// caller's actual create/flip/delete.
async fn write_marker(app: &App, owner: &str, name: &str, public: bool, meta: Option<(&str, &str, i64)>) {
    let existing = crate::index::read(&app.store.os, crate::index::Kind::Repo, owner, name).await;
    let (description, created_by, created_ms) = match meta {
        Some((d, by, at)) => (d.to_string(), by.to_string(), at),
        None => (
            existing.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
            existing.as_ref().map(|m| m.created_by.clone()).unwrap_or_default(),
            existing.as_ref().map(|m| m.created_ms).unwrap_or_else(|| crate::ownership::now_ms() as i64),
        ),
    };
    let m = crate::index::Marker {
        name: name.to_string(),
        public,
        created_by,
        created_ms,
        description,
        manifests: 0,
        updated_ms: 0,
    };
    if let Err(e) = crate::index::write(&app.store, crate::index::Kind::Repo, owner, &m).await {
        tracing::warn!(owner = %owner, repo = %name, error = %e, "index write");
    }
}

/// Create a repo ON THE NODE THAT OWNS IT, for the same reason `visibility` lives here: a second
/// process opening the repo's database while the owning node holds its own handle is two writers.
/// Routed by `api_route` like every other repo-scoped path, so the node that will serve the repo
/// is the node that creates it — and `App::route` claims a name the map does not yet know BEFORE
/// answering `Local`, so by the time `create_repo` opens the database this node holds its lease.
/// Nothing here opens the repo on the strength of "it does not exist yet".
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
    // Held across the claim as well as the marker write, because the name's uniqueness now rests
    // on THIS check-then-create rather than on a unique index in a central database. Two creates
    // of one name route to this node by repo key, so serializing them here is what makes exactly
    // one of them win; without the lock both pass `repo_exists` and the second silently wins the
    // repo the first was told it had. It is the same key `api_visibility` takes, so a
    // create-then-immediate-flip still cannot interleave its `set_public`/`write_marker`.
    let lock = app.store.keyed_lock(&crate::index::lock_key(crate::index::Kind::Repo, &owner, &name));
    let _guard = lock.lock().await;
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
            tracing::error!(owner = %owner, repo = %name, error = %msg, "create-repo");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not create repository").into_response();
        }
    }
    // Only after the repo exists, and only when asked for: `create_repo` leaves it private, so a
    // failure here leaves a private repo rather than a public one nobody meant to publish.
    if public {
        if let Err(e) = app.store.set_public(&owner, &name, true).await {
            return internal(e);
        }
    }
    // The repo's own database gets its metadata here, where the single writer is: the caller
    // passes the same creation instant it stamped on the index row, so the two cannot disagree
    // about when the repo was made.
    let created_at_ms = q
        .get("created_at_ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| crate::ownership::now_ms() as i64);
    let description = q.get("description").map(String::as_str).unwrap_or_default();
    let created_by = q.get("created_by").map(String::as_str).unwrap_or_default();
    if let Err(e) = app.store.set_repo_meta(&owner, &name, description, created_by, created_at_ms).await {
        return internal(e);
    }
    write_marker(&app, &owner, &name, public, Some((description, created_by, created_at_ms))).await;
    StatusCode::CREATED.into_response()
}

/// Edit a repo's description ON THE NODE THAT OWNS IT, for the same one-writer reason as
/// `create` and `visibility` beside it.
///
/// Authorization is the peer secret alone, exactly as `api_visibility` documents: whether the
/// human may edit this repo's settings is the api tier's question (`settings_caller` /
/// `may_act_under`), and this route is unreachable from the public listener.
pub(super) async fn api_description(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(description) = q.get("description").cloned() else {
        return (StatusCode::BAD_REQUEST, "description is required").into_response();
    };
    let Some((owner, name)) = crate::protocol::parse_repo_path(&format!("{owner}/{name}")) else {
        return (StatusCode::BAD_REQUEST, "invalid repository path").into_response();
    };
    // Asked of the object store, not the pool: `set_repo_description` goes through `db_for`,
    // which would CREATE a database for a name that never existed.
    if !app.store.repo_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.set_repo_description(&owner, &name, &description).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
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
    // Same lock key, and for the same reason as `api_create`/`api_visibility`: held across BOTH
    // the marker removal and the storage delete, so a concurrent flip cannot slip its
    // `write_marker` in between and leave a marker naming a repo that no longer exists.
    let lock = app.store.keyed_lock(&crate::index::lock_key(crate::index::Kind::Repo, &owner, &name));
    let _guard = lock.lock().await;
    // Markers removed BEFORE storage: gone from listings first, so a crash mid-delete never
    // leaves a marker pointing at a repo that no longer exists.
    if let Err(e) = crate::index::remove(&app.store, crate::index::Kind::Repo, &owner, &name).await {
        tracing::warn!(owner = %owner, repo = %name, error = %e, "index remove");
    }
    match app.store.delete_repo(&owner, &name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(owner = %owner, repo = %name, error = %e, "delete-repo");
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
    let result = if q.contains_key("remove") {
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
            tracing::error!(owner = %owner, repo = %name, error = %msg, "protect");
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
