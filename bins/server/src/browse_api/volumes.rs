//! Volume browse routes: the owner-scoped volume list and one volume's snapshot history.
//!
//! FROZEN (ruled: keep, don't delete). This was the USER-facing read side of the pre-cutover
//! `vol/{owner}/{name}` object-store registry; the agent-facing write side that used to fill it
//! (`vol_agent`) is deleted along with the object-store subsystem the commit model replaced.
//! Nothing writes this surface any more — the commit model's history lives in `Snapshot` CRs,
//! read through `/v1` — but old rows already written here still answer, so the Snapshots page
//! keeps reading it rather than going blank for pre-cutover volumes. Retire it once no
//! pre-cutover history is left to serve (see `deploy/k3s/README.md`'s cleanup section).

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
use kloudlite_git_core::httpx::Trusted;
use kloudlite_git_workspaces::registry::{volume_marker_prefix, VolExt};
use serde::Serialize;
use slatedb::object_store::ObjectStore;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Serialize)]
pub(super) struct VolumeSummary {
    /// The volume id — the workspace/environment id it was pushed from.
    name: String,
    /// Epoch millis of the volume's last push: the mtime of the `index/vol/{owner}/{name}` marker
    /// `append_snapshots` touches. `None` for a volume pushed only before the marker existed — the
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
    let names = match kloudlite_git_registry::list_dir_names(&app.store.os, &format!("{}/", crate::pool::path("vol", &owner))).await {
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

/// `GET /api/{owner}/{name}/volumehistory` — one volume's snapshots, newest first.
///
/// Repo-scoped in shape but routed by the VOLUME key (`vol/{owner}/{name}`, see `repo_of`), like
/// `imagetags` routes by the image key: the records live in that volume's own database and only the
/// node holding it may open it. Unlike `volumes` above, this one is allowed to open a database
/// precisely BECAUSE the ownership middleware has already sent it to the right node.
///
/// Answers the same `SnapshotRecord` shape `/vol-agent/{owner}/{name}/history` does — same records,
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
