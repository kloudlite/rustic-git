//! `/v1/environments` — create, list, read, delete, start/stop, clone, restore-to-new and
//! restore-in-place.

use super::scope::{find_env, may_act_on, may_allocate_for, mine, owned_by, resolve_new_owner, teams_for};
use super::volumes::{find_snapshot, volume_region};
use super::workspaces::{
    check_ws_name, clamp_quota, interrupted, interrupted_409, node_dead_warning, pushed_volumes,
    set_desired, storage_quota, CloneBody,
};
use super::{caller, check_region, environment_cost, guard_alloc, kube, kube_err, not_found, not_ready, phase, rid, ApiState};
use crate::crd::{self, DesiredState, VolumeSource};
use crate::k8s::{labels, ATTACHED_ENV_LABEL};
use crate::model::*;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::ResourceExt;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn env_volume(e: &crd::Environment) -> Option<&str> {
    e.status.as_ref().and_then(|st| st.volume_ref.as_deref()).filter(|v| !v.is_empty())
}

fn env_doc(e: &crd::Environment, pushed: &HashSet<String>) -> Environment {
    let id = e.name_any();
    let st = e.status.as_ref();
    Environment {
        owner: e.spec.owner.clone(),
        name: e.spec.name.clone(),
        region: e.spec.region.clone(),
        state: phase(st.map(|s| s.phase.as_str()), EnvState::Creating),
        placement: st.map(|s| s.node_name.clone()).filter(|n| !n.is_empty()),
        volume: env_volume(e)
            .filter(|v| pushed.contains(*v))
            .map(|_| format!("vol/{}/{id}", e.spec.owner)),
        services: e.spec.services.clone(),
        // Only `get_env` fills this in: it is a read of the CHILD volume's status, and a listing
        // that did it per row would be an N+1 against the API server for a field one page shows.
        restored_to: None,
        restore_requested_at: None,
        // Straight off the condition the reconciler writes, so the page shows the restore while it
        // is happening rather than a state that looks like an ordinary restart.
        restoring: st
            .and_then(|s| s.conditions.iter().find(|c| c.type_ == "Restoring" && c.status == "True"))
            .map(|c| c.reason.clone()),
        replicated: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Replicated").map(ConditionDoc::from)),
        degraded: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Degraded").map(ConditionDoc::from)),
        decommissioning: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Decommissioning").map(ConditionDoc::from)),
        id,
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct NewEnvironment {
    name: String,
    region: String,
    #[serde(default)]
    services: Vec<Service>,
    /// A team slug — makes this a team-owned environment, run on the team's bound node.
    /// `None`/equal to the caller means an ordinary personal environment.
    #[serde(default)]
    owner: Option<String>,
    /// An environment's subvolume holds every service's data. Defaults to the same 20 GB the web
    /// app sends for a workspace.
    #[serde(default = "default_env_quota")]
    quota_gb: u64,
}

fn default_env_quota() -> u64 {
    crd::DEFAULT_ENV_QUOTA_GB
}

/// The trust boundary for services: create and restore are the only routes that accept
/// caller-authored ones (`clone_env` copies an already-validated doc, and nothing updates services
/// in place), so a mount that gets past here is treated as trusted by a root agent from then on —
/// and a name that gets past here is what the controller applies, every requeue, forever.
fn check_services(services: &[Service]) -> Result<(), Response> {
    crate::model::validate_services(services).map_err(|e| (StatusCode::BAD_REQUEST, e).into_response())
}

/// The one place an `Environment` is written; `create_workspace`'s twin.
async fn create_environment(
    c: &kube::Client,
    id: &str,
    spec: crd::EnvironmentSpec,
) -> Result<crd::Environment, Response> {
    let l = labels(&spec.owner, "environment");
    let mut e = crd::Environment::new(id, spec);
    e.metadata.labels = Some(l);
    let api: Api<crd::Environment> = Api::all(c.clone());
    api.create(&PostParams::default(), &e).await.map_err(kube_err)
}

pub(crate) async fn create_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewEnvironment>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // Mounts name volumes (folders inside the env's own subvolume), not workspaces. The name is
    // joined onto the env's subvolume by a root agent, so it is a security boundary, not a
    // formality — see `validate_mount`. Checked before anything is written, deliberately.
    check_services(&body.services)?;
    check_ws_name(&body.name)?;
    check_region(&s, &body.region).await?;
    let owner = resolve_new_owner(&s, &caller_id, body.owner).await?;
    let c = kube(&s)?;
    let quota_gb = clamp_quota(&s, body.quota_gb);
    guard_alloc(&s, &owner, owner != caller_id.name, &environment_cost(quota_gb, body.services.len())).await?;
    let id = rid("env");
    let e = create_environment(
        c,
        &id,
        crd::EnvironmentSpec {
            owner,
            name: body.name,
            region: body.region,
            services: body.services,
            storage: Some(crd::WorkspaceStorage { quota_gb, source: None }),
            desired_state: DesiredState::Running,
            restore: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct RestoreEnvBody {
    name: String,
    snapshot_id: String,
    /// A team slug, resolved exactly as `NewEnvironment.owner` is — restoring a team's snapshot
    /// must produce a TEAM environment, or the restored copy is invisible to everyone but the
    /// person who clicked.
    #[serde(default)]
    owner: Option<String>,
    /// Validated exactly as `create_env`'s are — `check_services` is the trust boundary for mounts
    /// and a restore is just as much a caller-authored service list as a create is. Absent means
    /// "the services the snapshot froze", and so does an explicit `[]`: an environment always has
    /// services, so an empty list is not a way to ask for the data with nothing running (a
    /// snapshot that froze none is a 400).
    #[serde(default)]
    services: Option<Vec<Service>>,
    /// The region to RUN in. Where the snapshot's bytes live is the record's business, not this
    /// field's — that goes on the volume source.
    #[serde(default)]
    region: Option<String>,
    /// Absent means the snapshot's frozen quota, then the standard default.
    #[serde(default)]
    quota_gb: Option<u64>,
}

/// New environment grafted onto an explicit past snapshot — `restore_ws`'s twin, resolving the
/// snapshot the same way (server-tier history, caller/team scoping) and differing only in which
/// kind of object it writes. The agent needs no new path: `resolve_volume` already materializes a
/// `cloneOf { commit }` source for an Environment.
///
/// The services default to what the snapshot froze beside the bytes (`SnapshotState`), because an
/// environment's data without its services is not the environment. A non-empty body list overrides
/// it; an empty one means the same as none. An environment always has services.
pub(crate) async fn restore_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreEnvBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // A caller-authored list is refused before anything is read or written, as it always was; the
    // resolved list is checked again below, because it may instead come from the snapshot.
    if let Some(svcs) = &body.services {
        check_services(svcs)?;
    }
    // Named before anything is written, like `create_env`'s: an environment with no name is a row
    // nobody can tell apart from another.
    check_ws_name(&body.name)?;
    // The record's own region needs no check — it was checked when the environment was created,
    // and it is the one region guaranteed to hold these bytes. A caller's choice is checked like
    // a create's.
    if let Some(r) = &body.region {
        check_region(&s, r).await?;
    }
    // Restore-to-new is a clone at a named snapshot under the snapshot model — see
    // `restore_ws`'s matching comment. `find_snapshot` is the ownership
    // check: CR exists, Ready, and the caller may read `spec.owner`.
    let snap = find_snapshot(&s, &caller_id, None, &body.snapshot_id).await?;
    let (volume, src_owner) = (snap.spec.volume.clone(), snap.spec.owner.clone());
    // Twin of restore_ws's guard: a workspace's frozen state under an environment restore would
    // mount nothing and silently ignore the image/packages it froze. `None` stays "absent means
    // old". Checked before any other lookup, right after the fetch, same reasoning as restore_ws.
    let frozen = match &snap.spec.state {
        Some(crd::SnapshotState::Environment { services, quota_gb }) => Some((services.clone(), *quota_gb)),
        Some(crd::SnapshotState::Workspace { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "this snapshot was cut from a workspace; use POST /v1/workspaces/restore",
            )
                .into_response())
        }
        None => None,
    };
    // Defaults to the label the snapshot was FOUND under, not the caller: restoring a team's
    // environment produces a team environment without the client having to say so. Any OTHER
    // owner is refused even when the caller is a member of it: a snapshot found under team A is
    // A's data, and a restore into team B would carry it past A's membership boundary to everyone
    // in B. The caller's own account is the one legitimate elsewhere — their own copy.
    if body.owner.as_deref().is_some_and(|o| o != src_owner && o != caller_id.name) {
        return Err((StatusCode::FORBIDDEN, "a snapshot restores under its own owner, or under you").into_response());
    }
    let owner = resolve_new_owner(&s, &caller_id, body.owner.or(Some(src_owner.clone()))).await?;
    // The request, then what the snapshot froze, then nothing. A frozen list is DATA like any
    // other — `check_services` runs on whichever source won, because it is the trust boundary for
    // mounts and a hand-edited `state` is no more trusted than a request body.
    // An environment always has services: an empty body list is "use the snapshot's", never
    // "restore the data with nothing running" — the owner ruled the latter out on 2026-09-03.
    let services = body
        .services
        .clone()
        .filter(|l| !l.is_empty())
        .or_else(|| frozen.as_ref().map(|f| f.0.clone()))
        .unwrap_or_default();
    if services.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "an environment needs at least one service; this snapshot froze none, so pass `services`").into_response());
    }
    check_services(&services)?;
    let quota = match (body.quota_gb, &frozen) {
        (Some(q), _) => clamp_quota(&s, q),
        (None, Some(f)) => clamp_quota(&s, f.1),
        (None, None) => default_env_quota(),
    };
    let c = kube(&s)?;
    guard_alloc(&s, &owner, owner != caller_id.name, &environment_cost(quota, services.len())).await?;
    // The source environment may be long gone; the Volume holding the bytes still names its region.
    let region = match body.region {
        Some(r) => r,
        None => volume_region(c, &volume).await.unwrap_or_else(|| "default".to_string()),
    };
    let id = rid("env");
    let e = create_environment(
        c,
        &id,
        crd::EnvironmentSpec {
            owner,
            name: body.name,
            // No per-snapshot region under the snapshot model (see `restore_ws`'s matching comment).
            region,
            services,
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                source: Some(VolumeSource::CloneOf { volume, commit: Some(body.snapshot_id) }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct ListEnvQuery {
    /// Filter to one owner (a username or a team slug) — what the web app's `/{owner}/environments`
    /// page passes so a team page shows only that team's environments, not the caller's personal
    /// ones mixed in. Validated the same way `create_env`'s team owner is: caller must be that
    /// owner, or a member of it.
    #[serde(default)]
    owner: Option<String>,
}

pub(crate) async fn list_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListEnvQuery>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let owners: Vec<String> = match q.owner {
        Some(o) if may_act_on(&s, &caller_id, &o).await => vec![o],
        Some(_) => return Err(not_found()),
        None => {
            let mut owners = vec![caller_id.name.clone()];
            owners.extend(teams_for(&s, &caller_id.name).await);
            owners
        }
    };
    let c = kube(&s)?;
    let api: Api<crd::Environment> = Api::all(c.clone());
    let mut list = vec![];
    for owner in owners {
        let pushed = pushed_volumes(&s, c, &owner).await?;
        for e in mine(api.list(&owned_by(&owner)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner)) {
            list.push(env_doc(&e, &pushed));
        }
    }
    Ok(Json(list).into_response())
}

pub(crate) async fn get_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let pushed = pushed_volumes(&s, c, &e.spec.owner).await?;
    let mut doc = env_doc(&e, &pushed);
    // Which snapshot is CURRENT is the Volume's answer, not the history's: an in-place restore
    // makes an OLDER record the live one, and a page that assumed "newest = current" would then
    // offer to restore the snapshot the disk is already on.
    if let Some(v) = env_volume(&e) {
        let vols: Api<crd::Volume> = Api::all(c.clone());
        if let Some(st) = vols.get_opt(v).await.map_err(kube_err)?.and_then(|v| v.status) {
            doc.restored_to = st.restored_to;
            doc.restore_requested_at = st.restore_requested_at;
        }
    }
    Ok(Json(doc).into_response())
}

pub(crate) async fn start_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    if e.status.as_ref().is_some_and(|st| interrupted(&st.conditions)) {
        return Err(interrupted_409("environment"));
    }
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Running).await?;
    let pushed = pushed_volumes(&s, kube(&s)?, &e.spec.owner).await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &pushed))).into_response())
}

pub(crate) async fn stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Stopped).await?;
    let pushed = pushed_volumes(&s, kube(&s)?, &e.spec.owner).await?;
    let mut doc = env_doc(&e, &pushed);
    doc.state = EnvState::Stopped;
    let warning = e.status.as_ref().and_then(|st| node_dead_warning(&st.node_name, &st.conditions));
    let mut body = serde_json::to_value(&doc).expect("Environment doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

pub(crate) async fn delete_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let envs: Api<crd::Environment> = Api::all(c.clone());
    envs.delete(&id, &DeleteParams::default()).await.map_err(kube_err)?;
    // Only `/v1` writes spec, so clearing the attachments is this handler's job. Best-effort: the
    // reconciler treats a missing environment as unattached anyway, so a failure here degrades to a
    // stale field rather than a dangling grant.
    let wss: Api<crd::Workspace> = Api::all(c.clone());
    // NOT `owned_by(&e.spec.owner)`: `attach_ws` authorizes through `may_act_on`, which admits
    // team members, so a teammate's workspace can be attached to this environment while owned by
    // someone else entirely — an owner-scoped selector would miss it. `ATTACHED_ENV_LABEL` is the
    // view of `spec.attachedEnvironment` built for exactly this (`heal_attached_label` keeps it honest),
    // so it is the one selector that cannot miss an attached workspace regardless of who owns it.
    // The `Err` arm is LOGGED, not dropped: a failed list leaves workspaces pointing at a deleted
    // environment, and the reconciler treating that as unattached is a degradation somebody has
    // to be able to find in the logs.
    let attached_to = ListParams::default().labels(&format!("{ATTACHED_ENV_LABEL}={id}"));
    // Same warning shape `stop_ws` uses: a body the caller can act on, not just a log line only an
    // operator sees.
    let mut warning = None;
    match wss.list(&attached_to).await {
        Ok(list) => {
            let mut failed = 0;
            for w in list.items.iter().filter(|w| w.spec.attached_environment.as_deref() == Some(id.as_str())) {
                let patch = serde_json::json!({"spec": {"attachedEnvironment": serde_json::Value::Null}});
                if let Err(e) = wss.patch(&w.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await {
                    tracing::warn!(workspace = %w.name_any(), error = %e, "clearing an attachment");
                    failed += 1;
                }
            }
            if failed > 0 {
                warning = Some(format!("{failed} workspace(s) may still name this deleted environment"));
            }
        }
        Err(err) => {
            tracing::warn!(environment = %id, error = %err, "listing workspaces to clear attachments; some may still name this environment");
            warning = Some("could not list workspaces to clear; some may still name this deleted environment".to_string());
        }
    }
    let pushed = pushed_volumes(&s, c, &e.spec.owner).await?;
    let mut doc = env_doc(&e, &pushed);
    doc.state = EnvState::Deleted;
    let mut body = serde_json::to_value(&doc).expect("Environment doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// Env's local-copy route. Names no node, for the same reason `clone_ws` does not, and the
/// source's already-validated services carry over untouched.
pub(crate) async fn clone_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let src = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let new_id = rid("env");
    let volume = env_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
    // An environment clone COPIES bytes from the source's own live subvolume on the node that
    // holds it (`clone_local_ids`). An interrupted source's node is down, so there is nothing to
    // copy from and the clone would sit `Creating` forever — refused here, before anything is
    // created, rather than left as a workspace-shaped promise this path cannot keep. A workspace
    // clone of an interrupted source IS allowed: it grafts onto a replicated sync point instead.
    if src.status.as_ref().is_some_and(|st| interrupted(&st.conditions)) {
        return Err((
            StatusCode::CONFLICT,
            "the source environment is interrupted: its node is down, and an environment is copied from that node; cloning it waits for the node to return",
        )
            .into_response());
    }
    // No cut here, unlike `clone_ws`: the copy is taken from the live subvolume, so a snapshot
    // would be a CR nothing reads that retention sweeps a minute later. That is also why the
    // response carries no `based_on` — there is no cut this clone is based ON.
    //
    // ponytail: the ceiling is that an environment clone is LOCAL-ONLY. The upgrade is the
    // workspace's shared-worktree path (a `clone-{env}-{hex}` cut, `commit: Some(_)`, and the
    // `SnapshotPending` guard in this controller that `resolve_volume` would then need).
    //
    // `find_env` above admits a superadmin claim to reach any owner's environment (get, allowed);
    // the clone is the allocation, and that claim must not spend a team's quota it is not a
    // member of.
    if !may_allocate_for(&s, &caller_id, &src.spec.owner).await {
        return Err(not_found());
    }
    guard_alloc(&s, &src.spec.owner, src.spec.owner != caller_id.name, &environment_cost(quota, src.spec.services.len())).await?;
    let e = create_environment(
        c,
        &new_id,
        crd::EnvironmentSpec {
            owner: src.spec.owner.clone(),
            name: body.name,
            region: src.spec.region.clone(),
            services: src.spec.services.clone(),
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                // `None`: a fresh child Volume filled by a local copy, never a second worktree of
                // the source's volume. See the comment above `create_environment`.
                source: Some(VolumeSource::CloneOf { volume, commit: None }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

// ── push ─────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub(crate) struct RestoreInPlaceBody {
    snapshot_id: String,
}

/// Put a past snapshot back into THIS environment's own disk, rather than into a new one.
///
/// The API writes a wish and answers; the controllers do the work (scale the services down, swap
/// the subvolume, scale back up), which is why this is a 202 with no result to read. Everything
/// that could go wrong lives in the Environment's `Restoring` condition and the Volume's `Ready`.
///
/// The snapshot is resolved exactly as `restore_env`'s is — same `find_snapshot`, same caller/team
/// scoping — so "restore in place" can reach precisely the snapshots "restore into a new
/// environment" can, and a 404 still means "no such snapshot" and "not yours" alike.
pub(crate) async fn restore_env_in_place(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RestoreInPlaceBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let volume = env_volume(&e).ok_or_else(not_ready)?.to_string();
    // The wish names a `Snapshot` CR of this environment's OWN volume — validated Ready and
    // same-volume BEFORE the wish is written, so a bad id is a fast 4xx here rather than a silent
    // hang in `restore_gate` (which reads the wish uncritically, per its own doc comment).
    let snap = find_snapshot(&s, &caller_id, Some(&volume), &body.snapshot_id).await?;
    let (src_owner, volume) = (snap.spec.owner, snap.spec.volume);
    let wish = crd::RestoreWish {
        snapshot_id: body.snapshot_id,
        volume,
        owner: Some(src_owner),
        region: None,
        // What makes a repeat of the SAME snapshot a new wish: the controllers compare the id
        // against what is already live, so without this a second attempt after a failure would
        // look like a restore that had already happened.
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    // A merge patch: this touches one field of a spec the caller never sent the rest of.
    let api: Api<crd::Environment> = Api::all(kube(&s)?.clone());
    api.patch(&id, &PatchParams::default(), &Patch::Merge(&serde_json::json!({"spec": {"restore": wish}})))
        .await
        .map_err(kube_err)?;
    let mut doc = env_doc(&e, &HashSet::new());
    // The wish is written, so the answer says so: the reconciler's own condition takes a moment to
    // appear, and a body that still reads "running" makes the click look like it did nothing.
    doc.restoring = Some("Requested".into());
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}

// ── volumes ──────────────────────────────────────────────────────────────
//
// A snapshot is a point in time and outlives the workspace it was taken of, so none of these reads
// may hang off a live Workspace/Environment. The index and the records both live on the SERVER
// tier (`vol/{owner}/{name}`); the cluster is consulted only to answer "is the parent still
// around?", which is a display detail, never an authorization one.

#[cfg(test)]
mod tests {
    use super::check_services;
    use crate::crd;
    use crate::model::{Mount, Service};

    #[test]
    fn an_environment_doc_carries_degraded_and_decommissioning() {
        let mut e = crd::Environment::new(
            "env-1",
            crd::EnvironmentSpec {
                owner: "karthik".into(),
                name: "app".into(),
                region: "centralindia".into(),
                services: vec![],
                storage: None,
                desired_state: crd::DesiredState::Running,
                restore: None,
            },
        );
        e.status = Some(crd::EnvironmentStatus {
            conditions: vec![
                crd::condition("Degraded", true, "NodeDead", "node n1 is down", 4),
                crd::condition("Decommissioning", true, "NodeLeaving", "this node is being retired", 4),
            ],
            ..Default::default()
        });
        let d = super::env_doc(&e, &Default::default());
        assert_eq!(d.degraded.expect("degraded must be shown").reason, "NodeDead");
        let dec = d.decommissioning.expect("decommissioning must be shown");
        assert_eq!(dec.reason, "NodeLeaving");
        assert!(dec.message.contains("retired"));
    }

    fn svc(folder: &str, path: &str) -> Service {
        Service {
            name: "web".into(),
            image: "nginx".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![Mount { folder: folder.into(), path: path.into() }],
            ports: vec![],
        }
    }

    #[test]
    fn create_env_refuses_a_traversing_mount() {
        assert!(check_services(&[svc("data", "/data")]).is_ok());
        // The C1 payload: `{"folder": "/", "path": "/host"}` bind-mounts the host root RW into a
        // container whose image the same caller chose.
        for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
            assert!(check_services(&[svc(bad, "/host")]).is_err(), "folder {bad:?} must be refused");
        }
        assert!(check_services(&[svc("data", "/data:/etc")]).is_err(), "a ':' in path splices a mapping");
        assert!(check_services(&[svc("data", "relative")]).is_err());
    }
}
