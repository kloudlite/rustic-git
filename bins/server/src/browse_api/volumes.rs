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
use rustic_git_workspaces::registry::VolExt;
use serde::Serialize;
use slatedb::object_store::ObjectStore;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Serialize)]
pub(super) struct VolumeSummary {
    /// The volume id — the workspace/environment id it was pushed from.
    name: String,
    /// Epoch millis of the newest object under this volume's database prefix: the last time
    /// anything was written to it. It is the closest thing to "last snapshot" answerable without
    /// opening the database, which this handler must never do.
    /// ponytail: a compaction rewrites objects without a push, so this can run ahead of the real
    /// newest snapshot. Good enough to sort and date a list by; `volumehistory` has the exact
    /// times. The upgrade is an `index/` marker written once per push.
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
/// A volume's database lives under `repo/vol/{owner}/{name}/` (`pool::path` over
/// `registry::pool_coords`), so one LIST of that prefix names every volume without opening any of
/// them. A volume appears here once anything has been written to it, which is its first push.
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
    let prefix = slatedb::object_store::path::Path::from(format!("repo/vol/{owner}"));
    // Newest mtime per volume, accumulated in one pass over the listing: every object under a
    // volume's prefix belongs to that volume's database, and the segment after the prefix names it.
    let mut newest: BTreeMap<String, i64> = BTreeMap::new();
    let mut items = app.store.os.list(Some(&prefix));
    while let Some(item) = items.next().await {
        let meta = match item {
            Ok(m) => m,
            Err(e) => return internal(e.into()),
        };
        // `location` is `repo/vol/{owner}/{name}/...`; anything shallower is not a volume's data.
        let Some(rest) = meta.location.as_ref().strip_prefix(&format!("repo/vol/{owner}/")) else {
            continue;
        };
        let Some(name) = rest.split('/').next().filter(|n| !n.is_empty()) else { continue };
        let ms = meta.last_modified.timestamp_millis();
        newest.entry(name.to_string()).and_modify(|m| *m = (*m).max(ms)).or_insert(ms);
    }
    let out: Vec<VolumeSummary> = newest
        .into_iter()
        .map(|(name, ms)| VolumeSummary { name, latest_ms: (ms > 0).then_some(ms) })
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
