//! `cluster/settings`: the one document every central binary refreshes its `LiveSettings` from.
//!
//! Servable on ANY node, deliberately unlike a repo/image route: it is a shared object-store
//! document, not a per-repo database, so there is nothing to route by — the same exception
//! `_catalog` and `/api/{owner}/images` already are. `route_inner` (`router/route.rs`) answers it
//! locally, before `api_route` would otherwise 404 it as an unrecognised `/api/` tail — this path
//! carries no `BROWSE_TAILS` entry and needs none.
//!
//! Belt and braces on write: the peer secret proves "this is the admin server calling" (this
//! router is unreachable from the public listener), and the `superadmin` JWT claim proves "the
//! admin server itself checked the human is one" — the peer secret alone would let ANY
//! peer-authenticated caller (the worker, another server node) write settings, which only the
//! admin server should be able to trigger.
use crate::App;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use kloudlite_core::httpx::bearer_token;
use kloudlite_core::settings::{apply_patch, validate_stored, StoredCentralSettings, CENTRAL_SETTINGS_KEY};
use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt, PutPayload};
use std::sync::Arc;

fn key() -> OsPath {
    OsPath::from(CENTRAL_SETTINGS_KEY)
}

/// The current document, or the empty (all-default) one if the key has never been written.
async fn current(app: &App) -> Result<StoredCentralSettings, Response> {
    match app.store.os.get(&key()).await {
        Ok(r) => {
            let bytes = r.bytes().await.map_err(internal)?;
            serde_json::from_slice(&bytes).map_err(|e| {
                tracing::warn!(scope = "central", error = %e, "settings.invalid");
                (StatusCode::INTERNAL_SERVER_ERROR, "stored settings document is corrupt").into_response()
            })
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(StoredCentralSettings::default()),
        Err(e) => Err(internal(e)),
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "request.failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// The peer secret is already required to reach this handler at all (`peer_router`'s
/// `trust_peer` layer runs on every route it mounts, this one included) — what this checks is
/// the SECOND half: a `superadmin` bearer claim, so a peer-authenticated caller that is not the
/// admin server (the worker, another server node) still cannot write settings.
fn require_superadmin(app: &App, headers: &HeaderMap) -> Result<String, Response> {
    let Some(tok) = bearer_token(headers) else {
        return Err((StatusCode::UNAUTHORIZED, "bearer token required").into_response());
    };
    match app.jwt.verify(tok) {
        Ok(c) if c.superadmin => Ok(c.sub),
        Ok(_) => Err((StatusCode::FORBIDDEN, "superadmin only").into_response()),
        Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid token").into_response()),
    }
}

pub(crate) async fn get_settings(State(app): State<Arc<App>>) -> Response {
    match current(&app).await {
        Ok(doc) => Json(doc).into_response(),
        Err(r) => r,
    }
}

/// `PUT /api/admin/settings`. Body is a PARTIAL `StoredCentralSettings` — only the fields the
/// caller means to change. A revert is the same handler with the body set to a full
/// `history[n]` snapshot (Task 7's admin UI builds that body; there is no separate route).
pub(crate) async fn put_settings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(patch): Json<StoredCentralSettings>,
) -> Response {
    let updated_by = match require_superadmin(&app, &headers) {
        Ok(sub) => sub,
        Err(r) => return r,
    };
    if let Err(msg) = validate_stored(&patch) {
        return (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response();
    }
    let existing = match current(&app).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Millis since epoch as a string, not RFC3339 — this binary carries no `chrono` dependency,
    // and every other "when" in this crate (`created_ms`) is the same shape.
    let updated_at = crate::ownership::now_ms().to_string();
    let next = apply_patch(&existing, &patch, &updated_by, &updated_at);
    let bytes = match serde_json::to_vec(&next) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    if let Err(e) = app.store.os.put(&key(), PutPayload::from(bytes)).await {
        return internal(e);
    }
    // Applied locally too, so THIS node's own live handle sees it without waiting out
    // `SETTINGS_REFRESH_SECS` — the admin server that just wrote it may itself be forwarding on
    // behalf of a request that expects the change to be visible immediately.
    app.central.store(
        kloudlite_core::settings::CentralSettings::from_env().merged_with(&next),
    );
    Json(next).into_response()
}

/// `POST /api/admin/settings/revert`. No body: the target is always `history[0]` — "undo the
/// last write" — the same one the CLUSTER twin's `revert_cluster` names by index into its own
/// annotation-backed history, just with no index to pick since this route only ever means the
/// most recent entry. `apply_patch` with that snapshot as the patch reproduces that instant AND
/// pushes the current (pre-revert) document onto history as a new entry — same semantics as
/// every other write, so a revert can itself be reverted.
pub(crate) async fn revert_settings(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let updated_by = match require_superadmin(&app, &headers) {
        Ok(sub) => sub,
        Err(r) => return r,
    };
    let existing = match current(&app).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(snap) = existing.history.first() else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no history to revert to").into_response();
    };
    let patch: StoredCentralSettings = snap.into();
    let updated_at = crate::ownership::now_ms().to_string();
    let next = apply_patch(&existing, &patch, &updated_by, &updated_at);
    let bytes = match serde_json::to_vec(&next) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    if let Err(e) = app.store.os.put(&key(), PutPayload::from(bytes)).await {
        return internal(e);
    }
    app.central.store(
        kloudlite_core::settings::CentralSettings::from_env().merged_with(&next),
    );
    Json(next).into_response()
}
