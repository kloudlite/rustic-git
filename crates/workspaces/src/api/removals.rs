//! A removed member's pending cleanup, on demand: the listing and the admin "delete now".
//! Everything decides through `membership::judge` — this module only writes the `delete-now` mark
//! and a `delete-after` of NOW, which the cluster controller's GC acts on within a tick.
//!
//! The mark is written HERE, by the user-role process, because the admission policy
//! `kloudlite-removal-stamps-are-the-apis` admits only that process's account to either annotation;
//! a superadmin therefore uses this `/v1` route with their claim, and the admin process only lists.
//! The pair is judged once BEFORE the mark, so a removal the beat has not stamped yet is stamped now
//! (with its audit row) and a member who is back is cleared — a paused or active member always
//! answers 409. Deletion still obeys the region's `memberRemovalDeletes` (`ClusterSettings`).

use super::membership::{self, list_all, norm, stamp, system_annotation, team_pair, DELETE_AFTER, DELETE_NOW, REMOVED_AT};
use super::{caller, ApiState, Caller, Judged, TeamRole};
use crate::crd;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use kube::api::Api;
use kube::ResourceExt;
use serde_json::json;
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub struct Confirm {
    pub person: String,
    pub team: String,
}

/// A team admin of `team`, or a superadmin; a non-member learns nothing about the team.
async fn may_manage(s: &ApiState, c: &Caller, team: &str) -> Result<(), Response> {
    let Some(dir) = &s.directory else { return Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured").into_response()) };
    // The roster row, not the token claim: a revoked superadmin must not keep an irreversible reach.
    if c.superadmin {
        match dir.is_superadmin(&c.name).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(_) => return Err((StatusCode::SERVICE_UNAVAILABLE, "superadmin could not be checked").into_response()),
        }
    }
    match dir.team_role(&c.name, &norm(team)).await {
        Some(r) if r >= TeamRole::Admin => Ok(()),
        Some(_) => Err((StatusCode::FORBIDDEN, "only a team admin can do this").into_response()),
        None => Err(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(crate) async fn delete_now_route(
    State(s): State<Arc<ApiState>>,
    Path((team, owner)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Confirm>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // Authenticate before the body is judged, so an anonymous caller gets 401, not a 4xx on its JSON.
    let c = match caller(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match body {
        Ok(Json(body)) => delete_now(&s, &c, &team, &owner, &body).await,
        Err(e) => e.into_response(),
    }
}

pub(crate) async fn delete_now(s: &ApiState, c: &Caller, team: &str, owner: &str, body: &Confirm) -> Response {
    if !team_pair(owner, team) || norm(&body.person) != norm(owner) || norm(&body.team) != norm(team) {
        return (StatusCode::BAD_REQUEST, "confirm by naming the person and the team exactly").into_response();
    }
    if let Err(r) = may_manage(s, c, team).await {
        return r;
    }
    let (Some(k), Some(dir)) = (s.kube.as_ref(), s.directory.as_ref()) else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let (owner, team) = (norm(owner), norm(team));
    // Irreversible, so judged directly and fail CLOSED: a stale stamp on a re-added member must
    // never be marked, and an unreadable directory marks nothing.
    match dir.membership(&team, &owner).await {
        Ok(Judged::Member(_)) => return (StatusCode::CONFLICT, "still a member of the team").into_response(),
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "team membership could not be checked").into_response(),
        Ok(Judged::NotMember | Judged::TeamGone) => {}
    }
    membership::reconcile_pair(s, &owner, &team).await;
    let o = match list_all(k).await {
        Ok(o) => o,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    };
    let mine = |o_: &str, t: &str| norm(o_) == owner && norm(t) == team;
    // SSA under the membership manager: the body must repeat `removed-at`, or the apply drops it.
    let now = chrono::Utc::now().to_rfc3339();
    let mark = |m: &kube::core::ObjectMeta| system_annotation(m, REMOVED_AT).map(|at| json!({REMOVED_AT: at, DELETE_NOW: "true", DELETE_AFTER: now}));
    let mut marked = false;
    for x in o.benches.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)) {
        if let Some(a) = mark(&x.metadata) {
            marked |= stamp(&Api::<crd::Bench>::all(k.clone()), &x.name_any(), a).await;
        }
    }
    for x in o.workspaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)) {
        if let Some(a) = mark(&x.metadata) {
            marked |= stamp(&Api::<crd::Workspace>::all(k.clone()), &x.name_any(), a).await;
        }
    }
    for x in o.spaces.iter().filter(|x| mine(&x.spec.owner, &x.spec.team)) {
        if let Some(a) = mark(&x.metadata) {
            marked |= stamp(&Api::<crd::SpaceEnvironment>::all(k.clone()), &x.name_any(), a).await;
        }
    }
    if !marked {
        return (StatusCode::CONFLICT, "not pending removal").into_response();
    }
    let detail = json!({"owner": owner, "team": team, "by": c.name}).to_string();
    super::admin::audit(s, &c.name, "member.removed.delete_now", &format!("{team}/{owner}"), Some(detail), "ok").await;
    // Off means the mark stands and the controller's GC acts on it once the region turns deletes on.
    (StatusCode::ACCEPTED, Json(json!({"deletes_enabled": deletes_enabled(k).await}))).into_response()
}

/// The region's switch as the controller resolves it: `ClusterSettings` alone (`stored ?? false`),
/// since `memberRemovalDeletes` has no env source on either side. Unreadable answers off, which only
/// changes what the response says, never what is deleted.
async fn deletes_enabled(k: &kube::Client) -> bool {
    let stored = Api::<crd::ClusterSettings>::all(k.clone()).get_opt("default").await.ok().flatten().map(|c| c.spec).unwrap_or_default();
    crate::settings::AgentSettings::from_env().merged_with(&stored).member_removal_deletes
}

/// `GET /v1/teams/{slug}/removals`: handles and dates only, for the members table.
pub(crate) async fn team_removals(State(s): State<Arc<ApiState>>, Path(team): Path<String>, headers: HeaderMap) -> Response {
    let c = match caller(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Err(r) = may_manage(&s, &c, &team).await {
        return r;
    }
    match all(&s).await {
        Ok(rows) => {
            let team = norm(&team);
            Json(rows.into_iter().filter(|r| r.team == team).map(|r| json!({"owner": r.owner, "delete_at": r.delete_at})).collect::<Vec<_>>()).into_response()
        }
        Err(r) => r,
    }
}

pub(crate) async fn all(s: &ApiState) -> Result<Vec<membership::Removal>, Response> {
    let k = s.kube.as_ref().ok_or_else(|| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    let o = list_all(k).await.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e).into_response())?;
    Ok(membership::removals(&o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_test::{get, mock_client, not_found};

    const PATH: &str = "/apis/kloudlite.io/v1alpha1/clustersettings/default";

    /// One source: what `ClusterSettings` stores, else off — the same resolution the controller's
    /// GC acts on, so the response can never claim deletes the controller will not make.
    #[tokio::test]
    async fn the_switch_is_read_from_cluster_settings_alone() {
        let stored = |on: Option<bool>| {
            let mut spec = serde_json::json!({});
            if let Some(on) = on {
                spec["memberRemovalDeletes"] = on.into();
            }
            get(PATH, serde_json::json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "ClusterSettings", "metadata": {"name": "default"}, "spec": spec}))
        };
        assert!(deletes_enabled(&mock_client(vec![stored(Some(true))]).0).await);
        assert!(!deletes_enabled(&mock_client(vec![stored(Some(false))]).0).await);
        assert!(!deletes_enabled(&mock_client(vec![stored(None)]).0).await);
        assert!(!deletes_enabled(&mock_client(vec![not_found(PATH)]).0).await);
    }
}
