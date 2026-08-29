//! Volume browse routes: the owner-scoped volume list and one volume's snapshot history.
//!
//! These are the USER-facing read side of the `vol/{owner}/{name}` registry. The agent-facing
//! write side lives in `crate::vol_agent` and authenticates a region's agent token; that surface is
//! deliberately not reused here, because a per-region shared secret is not an authorization answer
//! for "may this person see these snapshots".
//!
//! A snapshot outlives the workspace it was taken of, so this is the only index of them that
//! survives the parent's deletion — which is exactly why the Snapshots page reads it rather than
//! enumerating live `Workspace`/`Environment` objects in the cluster.

use super::hidden;
use crate::router::internal;
use crate::App;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use rustic_git_core::httpx::Trusted;
use rustic_git_workspaces::registry::{volume_marker_prefix, VolExt};
use serde::Serialize;
use slatedb::object_store::ObjectStore;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Serialize)]
pub(super) struct VolumeSummary {
    /// The volume id — the workspace/environment id it was pushed from.
    name: String,
    /// Epoch millis of the volume's last push: the mtime of the `index/vol/{owner}/{name}` marker
    /// `append_commits` touches. `None` for a volume pushed only before the marker existed — the
    /// list still names it, and `volumehistory` has the exact times.
    latest_ms: Option<i64>,
}

/// `GET /api/{owner}/volumes` — every volume this owner has ever pushed, for the Snapshots page.
///
/// Owner-scoped like `images`, and for the same reason it carries the same warning: it reads the
/// shared object store ALONE. It must never call `vol_db`/`history`/`region`, each of which opens
/// one volume's database with no ownership check and would fence that volume's legitimate owner
/// when served on the wrong node. `repo_of` answers `None` for this path, so it is served by
/// whichever node receives it — that is only sound while this stays a pure object-store read.
///
/// Two LISTs, both O(volumes): a delimited one of `repo/vol/{owner}/` (`pool::path` over
/// `registry::pool_coords`) whose common prefixes are the names — a database lives there once
/// anything has been written to it, which is its first push — and one of the push markers for the
/// dates. It used to walk every SST and WAL object of every database under the owner, which grew
/// with pushes AND compactions.
pub(super) async fn volumes(
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
    let names = match rustic_git_registry::list_dir_names(&app.store.os, &format!("repo/vol/{owner}/")).await {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let marker_prefix = volume_marker_prefix(&owner);
    let mut dated: BTreeMap<String, i64> = BTreeMap::new();
    let mut markers = app.store.os.list(Some(&slatedb::object_store::path::Path::from(marker_prefix.as_str())));
    while let Some(item) = markers.next().await {
        let meta = match item {
            Ok(m) => m,
            Err(e) => return internal(e.into()),
        };
        if let Some(name) = meta.location.as_ref().strip_prefix(&marker_prefix) {
            dated.insert(name.to_string(), meta.last_modified.timestamp_millis());
        }
    }
    let out: Vec<VolumeSummary> = names
        .into_iter()
        .map(|name| VolumeSummary { latest_ms: dated.get(&name).copied().filter(|ms| *ms > 0), name })
        .collect();
    Json(out).into_response()
}

/// `DELETE /api/{owner}/{name}/volumedelete` — drop one volume's whole snapshot index.
///
/// Routed by the VOLUME key exactly like `volumehistory`, and for the same reason: the records
/// live in that volume's own database and only the node holding it may open it.
///
/// **What this deletes is the records and the refs, not the layer blobs.** A blob id is a per-push
/// uuid, but it is NOT private to the volume that minted it: `Engine::inherit`/`Engine::restore`
/// stage the SOURCE's lineage entries — same blob ids — under the destination, and the
/// destination's next push registers `CommitRecord`s naming them. So a clone or a restore makes
/// two volumes reference one blob, and deleting this volume's blobs would silently destroy the
/// other's snapshots. Answering "does any other volume still reference this blob" needs every
/// other volume's database, which this node may not open (that is the one invariant the whole
/// routing middleware exists for), so it cannot be answered here at all.
/// ponytail: layer blobs are orphaned by this, and reclaimed by nothing yet — every layer blob
/// carries a `layers/*.json` sidecar naming its parent, so a keep-biased sweep over `layers/`
/// that deletes only what NO volume's history references is the upgrade, and it belongs in the
/// worker beside `registry::gc`, not on this request path.
pub(super) async fn volumedelete(
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
    // Same guard as `volumehistory`, and the same reason: opening CREATES, so a delete of an
    // unknown name would mint the very ghost volume the listing then shows forever.
    if !app.store.vol_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.delete_volume(&owner, &name).await {
        Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/{owner}/{name}/snapshotdelete/{snapshot_id}` — drop ONE commit record.
///
/// Routed by the VOLUME key like `volumehistory` and `volumedelete`: it opens the same database.
///
/// Blobs are untouched, exactly as in `volumedelete` and for the same reason — a blob id is shared
/// with any volume cloned or restored from this one, and this node may not open theirs to find out.
/// An unknown id is a 404 with no side effect at all.
pub(super) async fn snapshotdelete(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, snapshot)): Path<(String, String, String)>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    // Before the open, always: opening a volume's database CREATES it.
    if !app.store.vol_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.delete_commit(&owner, &name, &snapshot).await {
        Ok(true) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Ok(false) => hidden(),
        Err(e) => internal(e),
    }
}

/// `GET /api/{owner}/{name}/volumehistory` — one volume's snapshots, newest first.
///
/// Repo-scoped in shape but routed by the VOLUME key (`vol/{owner}/{name}`, see `repo_of`), like
/// `imagetags` routes by the image key: the records live in that volume's own database and only the
/// node holding it may open it. Unlike `volumes` above, this one is allowed to open a database
/// precisely BECAUSE the ownership middleware has already sent it to the right node.
///
/// Answers the same `CommitRecord` shape `/vol-agent/{owner}/{name}/history` does — same records,
/// same order, different authentication (a person here, a region's agent there).
pub(super) async fn volumehistory(
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
    // Before the open, always: opening a volume's database CREATES it, so a probe of an unknown
    // name would mint a ghost volume that the listing above then shows forever.
    if !app.store.vol_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.history(&owner, &name).await {
        Ok(records) => Json(records).into_response(),
        Err(e) => internal(e),
    }
}
