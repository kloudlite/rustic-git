//! `/v1/bench` — a person's own bench in one team: create, start, stop, the tunnel token that
//! wakes a sleeping one, and attach. Answered ONLY to `spec.owner`: there is no superadmin arm
//! here and no admin route anywhere reads a bench, because a bench is a person's transcripts.
//!
//! Every rule lives in `my_bench`, so no handler can forget one: the team is the caller's handle
//! when absent (decision 4); membership decides `Full` or `ReadOnly` (decision 10) and a
//! non-member with no bench gets the same 404 as a stranger; the region is read from the
//! directory on every call and never stored on the Bench (decision 1). Waking is a `wakeAt`
//! patch — `/v1` is spec's only writer, and the agent compares it to `status.idleSince`
//! (decision 6).

use super::scope::may_allocate_for;
use super::workspaces::{check_attach, gateway_url, install_user_key_when, set_desired, AttachBody};
use super::{bench_cost, caller, check_region, guard_alloc, kube, kube_err, ApiState, Caller};
use crate::crd::{self, BenchAccess, DesiredState, Phase};
use crate::k8s::{labels, ATTACHED_ENV_LABEL, TEAM_LABEL};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct TeamQuery {
    team: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct NewBench {
    team: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// What the caller may do with this bench.
#[derive(Clone, Copy, PartialEq)]
enum Standing {
    Member,
    Departed,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"error": msg.into()}))).into_response()
}

fn no_team() -> Response {
    err(StatusCode::NOT_FOUND, "no such team")
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn bench_doc(b: &crd::Bench, region: &str) -> serde_json::Value {
    let st = b.status.clone().unwrap_or_default();
    json!({
        "id": b.metadata.name,
        "owner": b.spec.owner,
        "team": b.spec.team,
        "region": region,
        "model": b.spec.model,
        "desiredState": b.spec.desired_state,
        "access": b.spec.access,
        "phase": st.phase.as_str(),
        "nodeName": st.node_name,
        "conditions": st.conditions,
    })
}

/// The region `team` is bound to, from the directory; the 409 (team) or 422 (person, no `region`
/// given) otherwise. A person's first call with `region` binds it (`check_region`, then
/// `Directory::bind_region`). A `first` that differs from an existing binding is a 409.
async fn team_region(s: &ApiState, caller: &Caller, team: &str, first: Option<&str>) -> Result<String, Response> {
    let dir = s.directory.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no directory"))?;
    let personal = team == caller.name;
    let bound = match (dir.region_of(team).await, first) {
        (Some(r), _) => r,
        (None, _) if !personal => {
            return Err(err(StatusCode::CONFLICT, format!("team {team} has no region; a platform admin binds one")))
        }
        (None, None) => return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "choose a region for your personal bench")),
        (None, Some(r)) => {
            check_region(s, r).await?;
            match dir.bind_region(team, r).await {
                Ok(Some(b)) => b,
                Ok(None) => return Err(no_team()),
                Err(e) => {
                    tracing::warn!(owner = %team, error = %e, "bench.region.bind.failed");
                    return Err(err(StatusCode::SERVICE_UNAVAILABLE, "could not bind a region"));
                }
            }
        }
    };
    match first {
        Some(r) if r != bound => Err(err(StatusCode::CONFLICT, format!("team {team} is in region {bound}"))),
        _ => Ok(bound),
    }
}

/// caller, normalized team, region, standing, and the caller's own bench — or the 404 every other
/// case gets. `first` is only a create's body region (a person's first bind).
async fn my_bench(
    s: &ApiState,
    headers: &HeaderMap,
    team: Option<&str>,
    first: Option<&str>,
) -> Result<(Caller, String, String, Standing, Option<crd::Bench>), Response> {
    let caller = caller(s, headers).await?;
    let team = team.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).unwrap_or_else(|| caller.name.clone());
    let api: Api<crd::Bench> = Api::all(kube(s)?.clone());
    let bench = api
        .get_opt(&crd::bench_id(&caller.name, &team))
        .await
        .map_err(kube_err)?
        .filter(|b| b.spec.owner == caller.name);
    let standing = if may_allocate_for(s, &caller, &team).await {
        Standing::Member
    } else if bench.is_some() {
        Standing::Departed
    } else {
        return Err(no_team());
    };
    let region = team_region(s, &caller, &team, first).await?;
    Ok((caller, team, region, standing, bench))
}

/// The one writer of `spec.access` on the person's side: Full for a member, ReadOnly once departed.
async fn ensure_access(api: &Api<crd::Bench>, b: &mut crd::Bench, standing: Standing) -> Result<(), Response> {
    let want = if standing == Standing::Member { BenchAccess::Full } else { BenchAccess::ReadOnly };
    if b.spec.access != want {
        let name = b.metadata.name.clone().unwrap_or_default();
        api.patch(&name, &PatchParams::default(), &Patch::Merge(&json!({"spec": {"access": want}})))
            .await
            .map_err(kube_err)?;
        b.spec.access = want;
    }
    Ok(())
}

fn bench_api(s: &ApiState) -> Result<Api<crd::Bench>, Response> {
    Ok(Api::all(kube(s)?.clone()))
}

fn found(b: Option<crd::Bench>) -> Result<crd::Bench, Response> {
    b.ok_or_else(|| err(StatusCode::NOT_FOUND, "no bench"))
}

pub(crate) async fn get_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (_, _, region, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let mut b = found(b)?;
    ensure_access(&bench_api(&s)?, &mut b, standing).await?;
    Ok(Json(bench_doc(&b, &region)).into_response())
}

pub(crate) async fn create_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(body): Json<NewBench>,
) -> Result<Response, Response> {
    let (caller, team, region, standing, existing) =
        my_bench(&s, &headers, body.team.as_deref(), body.region.as_deref()).await?;
    if standing == Standing::Departed {
        return Err(no_team());
    }
    let api = bench_api(&s)?;
    if let Some(mut b) = existing {
        ensure_access(&api, &mut b, standing).await?;
        // Re-POSTing a stopped or idle bench starts a pod: an allocation, exactly as `start_bench`.
        if !crd::bench_wants_pod(&b) {
            guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
        }
        let name = b.metadata.name.clone().unwrap_or_default();
        // The image is re-stamped on every start, not only at create: a bench outlives a repin,
        // and one frozen at creation kept crashing on a build the fleet had already replaced
        // (hourly bench.start, 2026-09-22).
        let patch = json!({"spec": {"desiredState": DesiredState::Running, "wakeAt": now(), "image": bench_image()}});
        let b = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
        return Ok(Json(bench_doc(&b, &region)).into_response());
    }
    guard_alloc(&s, &caller.name, false, &bench_cost(&crd::PodResources::default())).await?;
    let id = crd::bench_id(&caller.name, &team);
    let image = bench_image();
    let mut b = crd::Bench::new(
        &id,
        crd::BenchSpec {
            owner: caller.name.clone(),
            team: team.clone(),
            image,
            model: body.model.filter(|m| !m.is_empty()).unwrap_or_else(|| crate::model::DEFAULT_BENCH_MODEL.to_string()),
            desired_state: DesiredState::Running,
            access: BenchAccess::Full,
            wake_at: None,
            resources: Default::default(),
            attached_environment: None,
        },
    );
    let mut l = labels(&caller.name, "bench");
    l.insert(TEAM_LABEL.to_string(), team.clone());
    b.metadata.labels = Some(l);
    let b = api.create(&PostParams::default(), &b).await.map_err(kube_err)?;
    tokio::spawn({
        let (s, c, owner, team, id) = (s.clone(), kube(&s)?.clone(), caller.name.clone(), team, id);
        async move {
            install_user_key_when::<crd::Bench>(&s, &c, &owner, &team, &id, |b| {
                b.status.as_ref().is_some_and(|st| !st.node_name.is_empty())
            })
            .await
        }
    });
    Ok((StatusCode::CREATED, Json(bench_doc(&b, &region))).into_response())
}

pub(crate) async fn start_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, _, _, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let mut b = found(b)?;
    let api = bench_api(&s)?;
    ensure_access(&api, &mut b, standing).await?;
    if !crd::bench_wants_pod(&b) {
        guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
    }
    let patch = json!({"spec": {"desiredState": DesiredState::Running, "wakeAt": now()}});
    api.patch(&b.metadata.name.clone().unwrap_or_default(), &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

pub(crate) async fn stop_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (_, _, _, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let mut b = found(b)?;
    ensure_access(&bench_api(&s)?, &mut b, standing).await?;
    set_desired::<crd::Bench>(kube(&s)?, &b.metadata.name.clone().unwrap_or_default(), DesiredState::Stopped).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Every tunnel connection asks here; an idle bench is woken and the client re-asks until 201.
pub(crate) async fn bench_session(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, _, region, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let mut b = found(b)?;
    let api = bench_api(&s)?;
    ensure_access(&api, &mut b, standing).await?;
    if b.spec.desired_state == DesiredState::Stopped {
        return Err(err(StatusCode::CONFLICT, "bench is stopped; start it"));
    }
    let id = b.metadata.name.clone().unwrap_or_default();
    let phase = b.status.as_ref().map(|st| st.phase).unwrap_or_default();
    if phase == Phase::Idle {
        // Waking re-charges cpu and memory, so it is an allocation like any start.
        guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
        api.patch(&id, &PatchParams::default(), &Patch::Merge(&json!({"spec": {"wakeAt": now()}})))
            .await
            .map_err(kube_err)?;
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": "waking"}))).into_response());
    }
    if phase != Phase::Ready {
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": phase.as_str()}))).into_response());
    }
    // `ensure_access` may just have demoted a departed member to ReadOnly while the phase still
    // describes the old Full pod. Only a Ready reason written for THIS access proves the pod being
    // dialled is the right one; the reconciler writes exactly `ReadOnly` or `Running`.
    let want = if b.spec.access == BenchAccess::ReadOnly { "ReadOnly" } else { "Running" };
    let serving = b.status.as_ref().is_some_and(|st| st.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True" && c.reason == want));
    if !serving {
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": Phase::Starting.as_str()}))).into_response());
    }
    let (token, claims) = s.jwt.mint_bench_session(&caller.name, &id, &region).map_err(|e| {
        tracing::error!(error = %e, "bench.session.mint.failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, "could not mint a session")
    })?;
    let expires_at = chrono::DateTime::from_timestamp(claims.exp as i64, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "token": token, "gateway": gateway_url(&region, &id), "expires_at": expires_at})),
    )
        .into_response())
}

pub(crate) async fn attach_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
    Json(body): Json<AttachBody>,
) -> Result<Response, Response> {
    let (caller, _, region, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    if standing == Standing::Departed {
        return Err(no_team());
    }
    let b = found(b)?;
    check_attach(&s, &caller, &body.environment, &region).await?;
    let patch = json!({
        "spec": {"attachedEnvironment": body.environment},
        "metadata": {"labels": {ATTACHED_ENV_LABEL: body.environment}},
    });
    bench_api(&s)?
        .patch(&b.metadata.name.clone().unwrap_or_default(), &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

pub(crate) async fn detach_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (_, _, _, standing, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    if standing == Standing::Departed {
        return Err(no_team());
    }
    let b = found(b)?;
    let patch = json!({
        "spec": {"attachedEnvironment": serde_json::Value::Null},
        "metadata": {"labels": {ATTACHED_ENV_LABEL: serde_json::Value::Null}},
    });
    bench_api(&s)?
        .patch(&b.metadata.name.clone().unwrap_or_default(), &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

fn bench_image() -> String {
    std::env::var("KLOUDLITE_BENCH_IMAGE")
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| crate::model::DEFAULT_BENCH_IMAGE.to_string())
}
