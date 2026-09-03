//! `/v1/volumes` — the snapshot history that outlives a workspace or environment: list, history,
//! refs, and the two deletes (one snapshot, or a whole detached volume with everything on it).
//!
//! A snapshot is a point in time and outlives the workspace it was taken of, so none of these reads
//! may hang off a live Workspace/Environment. The records are `Snapshot` CRs in the CLUSTER, not on
//! the server tier — there is no registry any more under the commit model; the cluster is consulted
//! a second time only to answer "is the parent still around?", which is a display detail, never an
//! authorization one.

use super::scope::{caller_owners, may_act_on, mine, owner_set_selector};
use super::{caller, check_path_segment, kube, kube_err, kube_unavailable, not_found, ApiState};
use crate::crd::{self, VolumeSource};
use kube::api::{Api, ListParams};
use kube::ResourceExt;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// The region the bytes actually live in, read off the detached `Volume` a restore grafts onto.
///
/// A restore's whole point is that the source workspace may be gone, and "default" is then a guess
/// that lands the new pod in a region whose nodes hold none of these snapshots.
pub(crate) async fn volume_region(c: &kube::Client, volume: &str) -> Option<String> {
    let vols: Api<crd::Volume> = Api::all(c.clone());
    // An unreadable Volume, or one written before regions existed (`region` is a plain String, so
    // "no region" is the empty one), leaves the caller's own fallback in charge.
    vols.get_opt(volume).await.ok().flatten().map(|v| v.spec.region).filter(|r| !r.is_empty())
}

#[derive(serde::Serialize)]
struct VolumeSummary {
    /// Registry name — the ws/env id, matching the `{owner}/{name}` the vol-agent surface and
    /// `RegistryClient` already key on.
    name: String,
    kind: String,
    /// `None` until the workspace/environment's first push writes a volume pointer. Always set
    /// now that this listing IS the pushed set, and kept because the web reads it.
    volume: Option<String>,
    /// What the source was called, from the newest record's provenance; the volume id when a
    /// record carries none (anything pushed before provenance existed).
    display_name: String,
    /// The workspace/environment is gone. The snapshots are not, and this listing is the only way
    /// back to them.
    deleted: bool,
    /// Epoch millis of the volume's last write. Approximate by construction — the newest
    /// `Snapshot` CR's creation time, sync points included.
    latest_ms: Option<i64>,
    /// How many pushes are on this volume. Any phase but `Error`, matching `cleanup_parent`'s
    /// rule: a push still being cut is a snapshot the person is waiting for, not one to hide.
    snapshots: u64,
    /// `readyAt` of the newest push, RFC3339; `None` while the only push is still being cut.
    last_push_at: Option<String>,
}

/// A live Workspace/Environment, reduced to what the volume routes need of it.
struct Parent {
    kind: String,
    display: String,
    /// `status.head` — the snapshot it is standing on, which `delete_snapshot` refuses to remove.
    head: Option<String>,
    /// The snapshot its SPEC was grafted onto. `head` only exists once the owning node has checked
    /// out; between the create and that first checkout the spec is the only record that this
    /// snapshot is load-bearing, and deleting it there is unrecoverable.
    base: Option<String>,
}

/// The snapshot a parent's volume source names, if any.
fn source_snapshot(storage: &Option<crd::WorkspaceStorage>) -> Option<String> {
    match storage.as_ref()?.source.as_ref()? {
        VolumeSource::CloneOf { commit, .. } => commit.clone(),
        VolumeSource::SeededFrom { snapshot, .. } => Some(snapshot.clone()),
        _ => None,
    }
}

/// The live parents, by the volume they are ON, with the kind they are. One list call per kind,
/// never one per row.
///
/// `None` means the cluster could not be asked, which is NOT the same as "nothing is alive": the
/// difference decides whether every row on the page is labelled "source deleted" during a kube
/// blip. The caller keeps `deleted: false` on `None` — the snapshots are what the page is for, and
/// they are all still there. The delete routes treat `None` as "cannot prove nothing is running"
/// and refuse, which is the opposite bias and the right one there.
///
/// Keyed on `status.volumeRef` where there is one, the parent's own name otherwise: a restored
/// environment holds a SECOND worktree on the source's volume, and keying on the name alone made
/// it invisible to both the listing and the head check.
///
/// Both kinds are selected by the caller's whole owner set, not just their own label: a team's
/// workspace is one they may read, and a head check that could not see it would let a delete take
/// a running worktree's base out from under it.
async fn live_parents(s: &ApiState, owners: &[String]) -> Option<BTreeMap<String, Parent>> {
    let c = s.kube.as_ref()?;
    let lp = ListParams::default().labels(&owner_set_selector(owners));
    let mut live = BTreeMap::new();
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    for w in mine(ws.list(&lp).await.ok()?.items, owners) {
        let name = w.name_any();
        let st = w.status.as_ref();
        let vol = st.and_then(|s| s.volume_ref.clone()).unwrap_or_else(|| name.clone());
        let parent = Parent {
            kind: "workspace".into(),
            display: w.spec.name.clone(),
            head: st.and_then(|s| s.head.clone()),
            base: source_snapshot(&w.spec.storage),
        };
        // A volume can carry several worktrees (a shared clone, a restore). The parent that OWNS
        // the volume — the one whose id is the volume's — is what names the row; anything else
        // would let a clone rename its source's listing.
        if name == vol {
            live.insert(vol, parent);
        } else {
            live.entry(vol).or_insert(parent);
        }
    }
    let envs: Api<crd::Environment> = Api::all(c.clone());
    for e in mine(envs.list(&lp).await.ok()?.items, owners) {
        let name = e.name_any();
        let st = e.status.as_ref();
        let vol = st.and_then(|s| s.volume_ref.clone()).unwrap_or_else(|| name.clone());
        let parent = Parent {
            kind: "environment".into(),
            display: e.spec.name.clone(),
            head: st.and_then(|s| s.head.clone()),
            base: source_snapshot(&e.spec.storage),
        };
        if name == vol {
            live.insert(vol, parent);
        } else {
            live.entry(vol).or_insert(parent);
        }
    }
    Some(live)
}

/// Every live Workspace/Environment ON `volume`, whoever owns it — the check both deletes make
/// before they take anything away.
///
/// UNLABELLED, unlike `live_parents`: a shared clone or a restore-to-new puts a worktree owned by a
/// DIFFERENT owner on the same volume (`CloneOf`), and an owner-scoped listing cannot see it. That
/// blind spot let a delete take another owner's running base out from under their pod, so this
/// listing is cluster-wide and matches on `status.volumeRef` (the parent's own name for a parent
/// that has not recorded one yet), exactly as the agent's own retention does.
///
/// `None` means the cluster could not be asked — "cannot prove nothing is running", which both
/// callers turn into a refusal rather than a delete.
async fn parents_of_volume(s: &ApiState, volume: &str) -> Option<Vec<Parent>> {
    let c = s.kube.as_ref()?;
    // Placed parents come back by the indexed field. Unplaced ones — created seconds ago, or
    // waiting on a node that is down — have no `volumeRef` at all, and the API server indexes that
    // as the empty string: a small, bounded set whose `spec.storage.source` says what they graft
    // onto. Both, because Task 5's protection depends on the second.
    let placed = ListParams::default().fields(&format!("status.volumeRef={volume}"));
    let unplaced = ListParams::default().fields("status.volumeRef=");
    let mut out = vec![];
    for lp in [&placed, &unplaced] {
        for w in Api::<crd::Workspace>::all(c.clone()).list(lp).await.ok()?.items {
            let st = w.status.as_ref();
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), w.name_any(), &w.spec.storage) {
                out.push(Parent {
                    kind: "workspace".into(),
                    display: w.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base: source_snapshot(&w.spec.storage),
                });
            }
        }
        for e in Api::<crd::Environment>::all(c.clone()).list(lp).await.ok()?.items {
            let st = e.status.as_ref();
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), e.name_any(), &e.spec.storage) {
                out.push(Parent {
                    kind: "environment".into(),
                    display: e.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base: source_snapshot(&e.spec.storage),
                });
            }
        }
    }
    Some(out)
}

/// On this volume: the node said so, the parent IS the volume (an owned one shares its id), or its
/// spec grafts onto it and no node has answered yet.
fn on_volume(volume: &str, vref: Option<String>, name: String, storage: &Option<crd::WorkspaceStorage>) -> bool {
    vref.unwrap_or(name) == volume
        || matches!(
            storage.as_ref().and_then(|s| s.source.as_ref()),
            Some(VolumeSource::CloneOf { volume: v, .. } | VolumeSource::SeededFrom { volume: v, .. }) if v == volume
        )
}

/// What a volume is, when nothing named it: no live parent, and a record written before provenance
/// existed (or backfilled). The ID PREFIX is authoritative — `rid("ws")` and `rid("env")` mint
/// every id there is, so an `env-` volume is an environment, full stop. Defaulting the whole class
/// to "workspace" filed every deleted environment's snapshots under the wrong heading.
fn kind_of(volume_id: &str) -> String {
    match volume_id.split_once('-').map(|(p, _)| p) {
        Some("env") => "environment",
        // `ws-`, and anything a future prefix has not taught this yet: a workspace is the common
        // case and the one a restore produces by default.
        _ => "workspace",
    }
    .to_string()
}

#[derive(serde::Deserialize)]
pub(crate) struct ListVolQuery {
    /// `workspace` or `environment`. The Environments page passes `environment` to find its
    /// archived rows; a workspace's snapshots are that one person's undo history and are reached
    /// only from their own workspace row.
    #[serde(default)]
    kind: Option<String>,
    /// One owner label — a username or a team slug. Same rule and same reason as `ListEnvQuery`'s:
    /// a team's page must show that team's archived rows, not the caller's personal ones mixed in.
    #[serde(default)]
    owner: Option<String>,
}

pub(crate) async fn list_volumes(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListVolQuery>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let owners = match &q.owner {
        Some(o) if may_act_on(&s, &caller_id, o).await => vec![o.clone()],
        Some(_) => return Err(not_found()),
        None => caller_owners(&s, &caller_id).await,
    };

    // The rows ARE the snapshots: one label-selected list, grouped by `spec.volume`. The registry
    // volume index is not consulted at all any more — a push writes a `Snapshot` CR and nothing
    // else, so a listing that read the index would have gone blind on everything pushed since.
    let api: Api<crd::Snapshot> = Api::all(kube(&s)?.clone());
    let snaps = mine(api.list(&ListParams::default().labels(&owner_set_selector(&owners))).await.map_err(kube_err)?.items, &owners);

    // The cluster answers only "does a parent still exist", so this degrades the page rather than
    // emptying it. `None` is an unanswered question, never an answer of "nothing": labelling every
    // row "source deleted" during a blip is the failure mode this distinction exists to prevent.
    let live = live_parents(&s, &owners).await;
    let known = live.is_some();
    let live = live.unwrap_or_default();

    let mut by_volume: BTreeMap<String, Vec<&crd::Snapshot>> = BTreeMap::new();
    for sn in &snaps {
        by_volume.entry(sn.spec.volume.clone()).or_default().push(sn);
    }

    let mut keep: Vec<VolumeSummary> = vec![];
    for (name, rows) in by_volume {
        // Any phase but `Error`, and never a sync point — `cleanup_parent`'s rule, so what keeps a
        // volume alive there is exactly what this counts.
        let pushes: Vec<&&crd::Snapshot> = rows
            .iter()
            .filter(|sn| {
                sn.is_snapshot() && sn.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
            })
            .collect();
        // A volume whose only records are sync points has nothing to show or restore — it is a
        // live worktree's replication state, not history, and was never a row on this page.
        if pushes.is_empty() {
            continue;
        }
        let owner = rows.first().map(|sn| sn.spec.owner.clone()).unwrap_or_default();
        let parent = live.get(&name);
        let kind = parent
            .map(|p| p.kind.clone())
            // The frozen `spec.state` is tagged by kind, so a deleted parent still says what it
            // was without a second read; the id prefix is the last resort for a legacy record.
            .or_else(|| {
                pushes.first().and_then(|sn| sn.spec.state.as_ref()).map(|st| match st {
                    crd::SnapshotState::Environment { .. } => "environment".to_string(),
                    crd::SnapshotState::Workspace { .. } => "workspace".to_string(),
                })
            })
            .unwrap_or_else(|| kind_of(&name));
        keep.push(VolumeSummary {
            kind,
            display_name: parent.map(|p| p.display.clone()).unwrap_or_else(|| name.clone()),
            deleted: known && parent.is_none(),
            volume: Some(format!("vol/{owner}/{name}")),
            latest_ms: rows.iter().filter_map(|sn| sn.creation_timestamp()).map(|t| t.0.as_millisecond()).max(),
            snapshots: pushes.len() as u64,
            last_push_at: pushes.iter().filter_map(|sn| sn.status.as_ref()?.ready_at.clone()).max(),
            name,
        });
    }

    if let Some(kind) = &q.kind {
        keep.retain(|v| &v.kind == kind);
    }
    keep.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(keep).into_response())
}

/// `DELETE /v1/volumes/{name}` — delete a volume and every snapshot on it. What the environment
/// Delete dialog calls when "Also delete its snapshots" is checked, and what an archived row's
/// "Delete snapshots" calls on its own.
///
/// A volume the caller may not read is a 404 rather than a 403 — they learn nothing about volumes
/// that are not theirs. A volume that still has a live parent is a 409: its bytes are somebody's
/// working copy, and deleting the Volume out from under a running worktree is not a snapshot
/// operation. Deleting the Volume CR takes every `Snapshot` on it (they are its children) and the
/// agent's byte sweep reclaims the subvolumes.
pub(crate) async fn delete_volume(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // The ownership check IS the snapshot listing: a volume with no `Snapshot` under a label the
    // caller may read is indistinguishable from one that does not exist.
    commit_model_snapshots(&s, &caller_id, &name).await?;
    // Deleting the Volume CR cascades to every Snapshot on it, so a volume carrying somebody
    // else's push is not this caller's to collect — the owner-filtered listing above cannot even
    // see those, which is how one team member's delete used to take the team's whole history.
    let owners: HashSet<String> = caller_owners(&s, &caller_id).await.into_iter().collect();
    if snapshots_on_volume(&s, &name).await?.iter().any(|sn| !owners.contains(&sn.spec.owner)) {
        return Err((
            StatusCode::CONFLICT,
            "this volume also holds snapshots owned by someone else; delete your own snapshots instead",
        )
            .into_response());
    }
    // A cluster that could not be listed is "cannot prove nothing is running" — the opposite bias
    // to the listing's, and the right one for a delete.
    if !parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?.is_empty() {
        return Err((StatusCode::CONFLICT, "the volume still has a workspace or environment").into_response());
    }
    delete_volume_cr(&s, &name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 404 on an already-gone object: two clicks on the same Delete button is a race, not an error.
async fn delete_volume_cr(s: &ApiState, name: &str) -> Result<(), Response> {
    let api: Api<crd::Volume> = Api::all(kube(s)?.clone());
    match api.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(kube_err(e)),
    }
}

/// `DELETE /v1/volumes/{name}/snapshots/{snapshot}` — delete ONE snapshot.
///
/// 404 for the two cases that must stay indistinguishable: a volume the caller may not read, and
/// an id that is not on it. 409 for the two that are refusals rather than absences: a sync point
/// (the agent owns those — deleting one by hand deletes a replica's send parent), and a snapshot a
/// running worktree is standing on.
///
/// Deleting the LAST snapshot of a volume nothing owns any more deletes the volume too: that is
/// what Task 1-3 detached it for, and leaving it behind would leak a subvolume nothing can ever
/// reach again.
pub(crate) async fn delete_snapshot(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((name, snapshot)): Path<(String, String)>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    check_path_segment(&snapshot)?;
    let items = commit_model_snapshots(&s, &caller_id, &name).await?;
    let target = items.iter().find(|sn| sn.name_any() == snapshot).ok_or_else(not_found)?;
    // `is_snapshot`, not `spec.transient`: a legacy migration baseline is a sync point by shape
    // rather than by flag, and deleting one by hand still removes a replica's send parent.
    if !target.is_snapshot() {
        return Err((StatusCode::CONFLICT, "a sync point cannot be deleted by hand").into_response());
    }
    let live = parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?;
    // EVERY parent on the volume, not just the caller's: a shared clone's worktree belongs to
    // another owner, and its head is just as much a running base as this owner's own.
    if live.iter().any(|p| p.head.as_deref() == Some(snapshot.as_str()) || p.base.as_deref() == Some(snapshot.as_str())) {
        return Err((StatusCode::CONFLICT, "this snapshot is the base of a running worktree").into_response());
    }
    let api: Api<crd::Snapshot> = Api::all(kube(&s)?.clone());
    match api.delete(&snapshot, &Default::default()).await {
        Ok(_) => {}
        // Already gone: someone got there first, which is the outcome the caller asked for.
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(kube_err(e)),
    }
    // The same rule `cleanup_parent` detached the Volume under (Task 2d), read from the other end:
    // it survives its parent only for as long as a snapshot needs it. Both halves are RE-READ
    // here rather than reused from above: a restore or a clone can attach a working copy, and
    // another push can land, in the window between those reads and this delete — deciding on the
    // stale view would delete a volume somebody just started using.
    let items = snapshots_on_volume(&s, &name).await?;
    let live = parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?;
    let remaining = items.iter().any(|sn| {
        sn.name_any() != snapshot
            && sn.is_snapshot()
            && sn.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
    });
    if !remaining && live.is_empty() {
        delete_volume_cr(&s, &name).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The commit-model reads for `/history` and `/refs`: `Snapshot` CRs instead of registry
/// records. Scoped by `caller_owners` exactly like `volume_owner` — a volume under a label the
/// caller may not read is a 404, same as the registry path.
async fn commit_model_snapshots(s: &ApiState, caller_id: &str, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    let items = commit_model_snapshots_maybe_empty(s, caller_id, name).await?;
    if items.is_empty() {
        return Err(not_found());
    }
    Ok(items)
}

/// `commit_model_snapshots`, minus the "no rows" 404 — F6: `/refs` on a workspace that has never
/// pushed has zero `Snapshot` CRs, which is a real, ownable volume with no commits yet, not an
/// unknown one; the registry path answers that with `{"main": null}`, never 404, and this is what
/// lets `volume_refs` match it.
async fn commit_model_snapshots_maybe_empty(s: &ApiState, caller_id: &str, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    check_path_segment(name)?;
    let owners: HashSet<String> = caller_owners(s, caller_id).await.into_iter().collect();
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    let list = api
        .list(&ListParams::default().fields(&format!("spec.volume={name}")))
        .await
        .map_err(kube_err)?;
    let mut items: Vec<crd::Snapshot> = list.items.into_iter().filter(|sn| owners.contains(&sn.spec.owner)).collect();
    // F2: NEWEST first, matching the registry path's order (`records.first()` is always its
    // tip) — a consumer rendering history the old way would show it backwards otherwise.
    items.sort_by_key(|sn| std::cmp::Reverse(sn.creation_timestamp().map(|t| t.0)));
    Ok(items)
}

/// Every snapshot on `volume`, whoever owns it — the same bias `parents_of_volume` takes, and for
/// the same reason: a restore or a shared clone puts another owner's snapshots on this volume, and
/// a decision that DESTROYS data must see them. The owner-filtered listings above stay what the
/// caller may read; this one is only ever counted, never returned.
async fn snapshots_on_volume(s: &ApiState, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    check_path_segment(name)?;
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    Ok(api
        .list(&ListParams::default().fields(&format!("spec.volume={name}")))
        .await
        .map_err(kube_err)?
        .items)
}

/// A single `Ready` commit-model snapshot, scoped by `caller_owners` exactly like
/// `commit_model_snapshots`. A 404 here is "unknown", "not yours", "not this volume's" (when
/// `volume` is given), or "not cut yet" alike — the caller only needs to know it cannot restore
/// onto it, never which of those it was — "no such snapshot" and "not yours" collapse into one
/// 404, same as everywhere else in this API.
///
/// `Some(volume)` is restore-in-place, which already knows the volume and must stay on it;
/// `None` is restore-to-new (`restore_ws`/`restore_env`), where the snapshot id is all the client
/// has and `spec.volume` is exactly what this resolves.
pub(crate) async fn find_snapshot(
    s: &ApiState,
    caller_id: &str,
    volume: Option<&str>,
    snapshot_id: &str,
) -> Result<crd::Snapshot, Response> {
    check_path_segment(snapshot_id)?;
    let owners: HashSet<String> = caller_owners(s, caller_id).await.into_iter().collect();
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    let snap = api.get_opt(snapshot_id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    let ready = snap.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready);
    if volume.is_some_and(|v| snap.spec.volume != v) || !owners.contains(&snap.spec.owner) || !ready {
        return Err(not_found());
    }
    Ok(snap)
}

/// Every row is a SNAPSHOT. A sync point is the agent's replication state, not something the
/// person took, and the migration baseline is not either — showing them as history offers a
/// restore onto a record that can vanish on the next sync beat.
fn snapshot_rows(items: &[crd::Snapshot]) -> Vec<serde_json::Value> {
    items
        .iter()
        .filter(|sn| sn.is_snapshot())
        .map(|sn| {
            let phase = sn.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Pending);
            serde_json::json!({
                "id": sn.name_any(),
                "state": serde_json::to_value(&sn.spec.state).unwrap_or(serde_json::Value::Null),
                // Both dead weight now — a `Snapshot` CR carries neither — but `ApiCommitRecord` in
                // web/apps/web/src/lib/api.ts still declares them, so they stay on the wire until a
                // web change drops the fields.
                "lineage": [],
                "region": "",
                "message": sn.spec.message,
                // F3: `jiff::Timestamp`'s `Display` IS RFC3339 (`2026-01-01T00:00:00Z`), the
                // same shape `chrono::DateTime<Utc>`'s serde impl gives the registry path's
                // `created_at` — asserted directly in `history_created_at_is_rfc3339`
                // rather than trusted, since a jiff/chrono formatting drift would be silent otherwise.
                "createdAt": sn.creation_timestamp().map(|t| t.0.to_string()),
                "parent": if sn.spec.parent.is_empty() { serde_json::Value::Null } else { serde_json::json!(sn.spec.parent) },
                "phase": phase.as_str(),
            })
        })
        .collect()
}

pub(crate) async fn volume_history(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let items = commit_model_snapshots(&s, &caller_id, &name).await?;
    Ok(Json(snapshot_rows(&items)).into_response())
}

/// There is exactly one ref per volume ("main") and its value is always the newest snapshot — the
/// same "first = tip" convention `engine::ops` relies on.
pub(crate) async fn volume_refs(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // F6: never 404 here — a zero-commit volume is `{"main": null}`.
    let items = commit_model_snapshots_maybe_empty(&s, &caller_id, &name).await?;
    // Never a sync point: `main` is what a clone or a restore grafts onto, and retention deletes
    // every sync point but the newest.
    let tip = items.iter().find(|sn| sn.is_snapshot()).map(|sn| sn.name_any());
    Ok(Json(serde_json::json!({"main": tip})).into_response())
}
