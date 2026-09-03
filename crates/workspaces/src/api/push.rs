//! `push` — the single mutating verb: a `Snapshot` CR, and the clone/restore machinery that grafts
//! a new working copy onto one.

use super::{caller, guard_alloc, kube, kube_err, not_ready, ApiState};
use super::scope::{find_env, my_ws};
use super::workspaces::ws_volume;
use super::environments::env_volume;
use crate::crd;
use kube::api::{Api, ListParams, PostParams};
use kube::{Resource, ResourceExt};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use std::collections::HashSet;
use std::sync::Arc;

/// What a clone was grafted onto. Always present on a clone response: a clone is always based on
/// a cut, and only the interrupted case makes that cut older than "now" — which is the one thing a
/// person needs to weigh before accepting it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BasedOn {
    pub snapshot: String,
    /// The cut's `readyAt` — absent for a cut this request just made, which is not Ready yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// How stale the copy is, in seconds, at the moment of this response. Zero for a fresh cut.
    pub age_seconds: i64,
    /// The source's node was down, so this is the newest cut a peer already HOLDS rather than a
    /// fresh one taken for this request.
    pub interrupted: bool,
}

/// Every `Snapshot` of `volume`, and the newest Ready transient of `worktree` among them as the
/// whole object: a clone's parent when the owner can cut, and the clone's own base when it cannot.
/// `crd::newest_transient_of` is the ordering, shared with the agent so `/v1` and placement can
/// never disagree about which cut is newest.
async fn newest_transient(c: &kube::Client, volume: &str, worktree: &str) -> Result<(Option<crd::Snapshot>, Vec<crd::Snapshot>), Response> {
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let list = api
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(kube_err)?;
    let newest = crd::newest_transient_of(&list.items, worktree);
    let found = newest.and_then(|n| list.items.iter().find(|s| s.name_any() == n).cloned());
    Ok((found, list.items))
}

/// Same one-cut-in-flight rule `create_snapshot` enforces: a second Working cut of one worktree
/// forks the transient chain, and the loser then misdescribes what it holds.
///
/// Only where a cut is about to be TAKEN. An interrupted source's Working cut belongs to the node
/// that died holding it: it will never converge and nothing will ever clear it, so refusing on it
/// would close the one door left open — cloning off a copy a peer already holds.
fn refuse_cut_in_flight(all: &[crd::Snapshot], worktree: &str) -> Result<(), Response> {
    if all.iter().any(|sn| sn.spec.worktree == worktree && sn.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Working)) {
        return Err((StatusCode::CONFLICT, "a snapshot is already being cut for this workspace").into_response());
    }
    Ok(())
}

/// Every transient of `worktree` that some node's `VolumeReplica` reports HOLDING — the candidate
/// set a clone of an interrupted source may graft onto, because its own node cannot serve a byte.
/// Read-only, and only here: `status.branches` is the pulling agent's to write, always.
async fn replicated_transients(c: &kube::Client, volume: &str, worktree: &str) -> Result<HashSet<String>, Response> {
    let api: Api<crd::VolumeReplica> = Api::all(c.clone());
    Ok(api
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter_map(|r| r.status.and_then(|st| st.branches.get(worktree).cloned()))
        .collect())
}

/// Seconds between `at` and now, floored at 0. An unparseable or absent timestamp is 0: the age is
/// advisory, and a clone must never fail because a clock string did not parse.
fn age_seconds(at: Option<&str>) -> i64 {
    at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0))
        .unwrap_or(0)
}

/// What this clone grafts onto, and — in the ordinary case — the act of creating it.
///
/// The cut is taken HERE rather than left to the next sync beat because a clone that leaned on the
/// last beat could be a whole `WS_SYNC_SECS` stale: silent data loss nobody asked for. It is a
/// `clone-{worktree}-{hex}` TRANSIENT, the same shape the sync beat produces, so the puller sends a
/// delta against what a replica already holds and retention sweeps it like any other sync point.
///
/// An INTERRUPTED source is the one exception: its node is down, so nothing can be cut there at
/// all. The clone then grafts onto the newest transient — which, by the up-to-date rule, is exactly
/// the one an up-to-date node holds — and the response states its age so the person chooses the
/// gap knowingly. With no transient anywhere there is nothing to graft onto and no way forward.
/// Returns the cut UNCREATED (`Some`) in the ordinary case: the caller creates the workspace first
/// and only then writes it. A create that fails after the cut leaves a `Working` Snapshot nothing
/// will ever fulfil, which then blocks the next clone on the one-cut-in-flight guard.
pub(crate) async fn clone_base(
    c: &kube::Client,
    owner: &str,
    volume: &str,
    worktree: &str,
    interrupted: bool,
    parent_ref: Option<OwnerReference>,
    state: crd::SnapshotState,
) -> Result<(BasedOn, Option<crd::Snapshot>), Response> {
    let (newest, all) = newest_transient(c, volume, worktree).await?;
    if interrupted {
        // Not the newest transient cluster-wide — the newest one another node actually HOLDS. The
        // owner is down, so a cut it turned Ready seconds before it died may exist nowhere else at
        // all, and grafting onto that leaves the clone unplaceable forever. `status.branches` on a
        // `VolumeReplica` is the only record of who holds what, and the up-to-date rule placement
        // applies reads exactly the same field.
        let held = replicated_transients(c, volume, worktree).await?;
        let newest_held = crd::newest_transient_of(&all.iter().filter(|s| held.contains(&s.name_any())).cloned().collect::<Vec<_>>(), worktree);
        let held = newest_held.and_then(|n| all.into_iter().find(|s| s.name_any() == n)).ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "the source's node is down and no other node holds a sync point of it yet; nothing can be cloned until one syncs or the node returns",
            )
                .into_response()
        })?;
        let at = held.status.as_ref().and_then(|st| st.ready_at.clone());
        return Ok((BasedOn { snapshot: held.name_any(), age_seconds: age_seconds(at.as_deref()), at, interrupted: true }, None));
    }
    refuse_cut_in_flight(&all, worktree)?;
    let name = format!("clone-{worktree}-{}", crd::short_hex());
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: volume.to_string(),
            owner: owner.to_string(),
            worktree: worktree.to_string(),
            // The previous sync point, so the puller sends a delta. Empty on a source that has
            // never been snapshotted at all, exactly as a root snapshot is.
            parent: newest.map(|s| s.name_any()).unwrap_or_default(),
            message: Some("cloning".to_string()),
            transient: true,
            state: Some(state),
        },
    );
    // `status` on CREATE is stored verbatim, which is how this is born `Working` and reaches the
    // owning node's snapshot reconciler rather than sitting at the schema's `Pending` default.
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    snap.metadata.labels = Some(crd::snapshot_labels(owner, volume));
    // Owned by the source parent, exactly as the sync beat's cuts are: deleting the source is the
    // whole delete, and a cut nothing points at would otherwise outlive it as a leaked subvolume.
    snap.metadata.owner_references = parent_ref.map(|r| vec![r]);
    Ok((BasedOn { snapshot: name, at: None, age_seconds: 0, interrupted: false }, Some(snap)))
}

/// Attach `based_on` to a doc the way `stop_ws` attaches `warning`: a key beside the doc's own
/// fields, so the web client's `res.json()` of a Workspace/Environment keeps working unchanged.
pub(crate) fn with_based_on<T: serde::Serialize>(doc: &T, based_on: &BasedOn) -> Response {
    let mut body = serde_json::to_value(doc).expect("doc always serializes");
    body["based_on"] = serde_json::to_value(based_on).expect("BasedOn always serializes");
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

#[derive(serde::Deserialize, Default)]
struct PushBody {
    message: Option<String>,
}

/// The push body is optional (`{message?}`), and axum's `Json<T>` extractor 415s a request
/// with no body/content-type at all rather than treating it as absent — so the message is read
/// as raw bytes and parsed only when present, same forgiving shape a curl with no `-d` expects.
fn optional_push_message(body: axum::body::Bytes) -> Result<Option<String>, Response> {
    if body.is_empty() {
        return Ok(None);
    }
    let parsed: PushBody = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid push body").into_response())?;
    Ok(parsed.message)
}

pub(crate) async fn push_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let msg = optional_push_message(body)?;
    let volume = ws_volume(&w).ok_or_else(not_ready)?;
    let owner_of = if w.spec.team.is_empty() { w.spec.owner.clone() } else { w.spec.team.clone() };
    guard_alloc(&s, &owner_of, !w.spec.team.is_empty(), &[(crate::quota::Dim::Snapshots, 1)]).await?;
    let head = w.status.as_ref().and_then(|st| st.head.clone());
    let state = crd::SnapshotState::of_workspace(&w);
    create_snapshot(kube(&s)?, volume, &w.spec.owner, &id, head, msg, state).await
}

pub(crate) async fn push_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let msg = optional_push_message(body)?;
    let volume = env_volume(&e).ok_or_else(not_ready)?;
    guard_alloc(&s, &e.spec.owner, e.spec.owner != caller_id, &[(crate::quota::Dim::Snapshots, 1)]).await?;
    let head = e.status.as_ref().and_then(|st| st.head.clone());
    let state = crd::SnapshotState::of_environment(&e);
    create_snapshot(kube(&s)?, volume, &e.spec.owner, &id, head, msg, state).await
}

/// A `Snapshot` CR, created `Working` so the agent's `reconcile_snapshot` can act on the very first
/// pass — CR-first (module doc).
async fn create_snapshot(
    c: &kube::Client,
    volume: &str,
    owner: &str,
    worktree: &str,
    parent: Option<String>,
    message: Option<String>,
    state: crd::SnapshotState,
) -> Result<Response, Response> {
    // F1: two pushes of the same worktree before the first is cut both read the same `head` and
    // both claim it as `parent` — the loser becomes a Ready snapshot no worktree's `head` ever
    // reaches, and `worktree_heads`/retention only walks the WINNER's chain, so the loser is never
    // revisited: an unbounded CR+disk leak, with a `parent` that misdescribes what it holds. A
    // worktree may have at most one `Working` cut in flight at a time — refuse the second here,
    // before it exists, rather than reconcile two winners later. Same guard `clone_base` uses
    // (`refuse_cut_in_flight`), whose own `// ponytail:` names the TOCTOU sliver this still has.
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let all = api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await.map_err(kube_err)?.items;
    refuse_cut_in_flight(&all, worktree)?;
    let name = crd::snapshot_name(volume);
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: volume.to_string(),
            owner: owner.to_string(),
            worktree: worktree.to_string(),
            parent: parent.unwrap_or_default(),
            message,
            // Not transient: a push IS a snapshot (`Snapshot::is_snapshot`). It is kept until
            // someone deletes it by hand (`delete_snapshot`), never pruned by retention, and it
            // keeps its Volume alive after the workspace is gone (`cleanup_parent`).
            transient: false,
            state: Some(state),
        },
    );
    snap.metadata.labels = Some(crd::snapshot_labels(owner, volume));
    // `status` on CREATE is stored verbatim (the subresource split only governs UPDATE/PATCH), so
    // this is how the object is born `Working` instead of the schema's `Pending` default —
    // `reconcile_snapshot` only ever acts on `Working`.
    // Owned by the Volume so the snapshot record goes with it: a Snapshot CR with no owner outlived
    // its deleted workspace once, and its snapshot subvolume sat on a node with nothing left to
    // reap it. The agent's own cuts (sync points, stops) are owned the same way, via the parent.
    let vol = match Api::<crd::Volume>::all(c.clone()).get(volume).await {
        Ok(v) => v,
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            return Err((StatusCode::NOT_FOUND, "the volume this worktree is on no longer exists").into_response())
        }
        Err(e) => return Err(kube_err(e)),
    };
    snap.metadata.owner_references =
        Some(vec![vol.controller_owner_ref(&()).expect("a live Volume has a uid")]);
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    api.create(&PostParams::default(), &snap).await.map_err(kube_err)?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id": name, "phase": crd::Phase::Working.as_str()}))).into_response())
}
