//! `/v1/me/environments`: which environment each of the CALLER's spaces follows — one per team they
//! are in, plus their personal space (`team` = their handle). The only writer of
//! `SpaceEnvironment`; the agent converges every pod of the space from it (`controller::space`).
//!
//! The person is always the caller: nothing here names another person, and a body carrying
//! `owner` is refused outright (400) rather than ignored, the same rule as `/v1/keys`. Region is
//! deliberately not checked — a space spans regions, an environment does not, and a pod elsewhere
//! reports `Attached=False/RegionMismatch` itself. See
//! `docs/superpowers/specs/2026-09-14-person-environment-design.md`.

use super::*;
use super::scope::teams_for;


#[derive(serde::Serialize)]
pub(crate) struct SpaceDoc {
    team: String,
    environment: String,
    region: Option<String>,
}


/// The team segment as the caller's space: their own handle is the personal space, any other
/// value must be a team they are a MEMBER of — a superadmin claim chooses nothing for anyone.
/// Everything else is a 404: the caller learns nothing about teams they are not in.
async fn space_team(s: &ApiState, caller: &Caller, team: &str) -> Result<String, Response> {
    let team = team.to_lowercase();
    if team == caller.name.to_lowercase() || teams_for(s, &caller.name).await.iter().any(|t| t.eq_ignore_ascii_case(&team)) {
        return Ok(team);
    }
    Err(not_found())
}


fn spaces(s: &ApiState) -> Result<Api<crd::SpaceEnvironment>, Response> {
    Ok(Api::all(kube(s)?.clone()))
}


pub(crate) async fn list_my_environments(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    let c = kube(&s)?;
    let lp = ListParams::default().labels(&format!("{}={}", crate::k8s::OWNER_LABEL, caller.name.to_lowercase()));
    // The label is a view; `spec.owner` is the answer.
    let mine: Vec<crd::SpaceEnvironment> =
        spaces(&s)?.list(&lp).await.map_err(kube_err)?.items.into_iter().filter(|x| x.spec.owner.eq_ignore_ascii_case(&caller.name)).collect();
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let mut out = Vec::with_capacity(mine.len());
    for x in mine {
        let region = envs.get_opt(&x.spec.environment).await.map_err(kube_err)?.map(|e| e.spec.region);
        out.push(SpaceDoc { team: x.spec.team, environment: x.spec.environment, region });
    }
    Ok(Json(out).into_response())
}


pub(crate) async fn set_my_environment(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(team): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    if body.get("owner").is_some() {
        return Err((StatusCode::BAD_REQUEST, "a space is always the caller's own; the body carries no owner").into_response());
    }
    let Some(environment) = body.get("environment").and_then(|v| v.as_str()).map(str::to_string) else {
        return Err((StatusCode::BAD_REQUEST, "environment is required").into_response());
    };
    // Patched verbatim into a label value.
    if !crate::model::valid_segment_label(&environment) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "invalid environment id").into_response());
    }
    let team = space_team(&s, &caller, &team).await?;
    let envs: Api<crd::Environment> = Api::all(kube(&s)?.clone());
    // `visible_env`: the hidden builder is indistinguishable from an id that does not exist.
    let e = envs.get_opt(&environment).await.map_err(kube_err)?.filter(environments::visible_env).ok_or_else(not_found)?;
    if !e.spec.owner.eq_ignore_ascii_case(&team) {
        return Err((StatusCode::CONFLICT, format!("that environment is not {team}'s; a space may use only its own team's environments")).into_response());
    }
    let obj = crd::space_environment(&caller.name, &team, &environment);
    let name = obj.metadata.name.clone().unwrap_or_default();
    spaces(&s)?
        .patch(&name, &PatchParams::apply(crd::API_FIELD_MANAGER).force(), &Patch::Apply(&obj))
        .await
        .map_err(kube_err)?;
    Ok(Json(SpaceDoc { team, environment, region: Some(e.spec.region) }).into_response())
}


/// Idempotent: a space with no environment is already the state asked for. Named from the caller,
/// so it can only ever clear the caller's own choice — including one left in a team they have
/// since left, which is why membership is not checked here.
pub(crate) async fn clear_my_environment(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(team): Path<String>,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    match spaces(&s)?.delete(&crd::space_name(&caller.name, &team), &Default::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(kube_err(e)),
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}


/// The retired per-workspace and per-bench attach/detach routes, for one release: a stale client
/// fails loudly instead of silently attaching nothing.
// ponytail: delete with the routes next release.
pub(crate) async fn attach_gone() -> Response {
    (StatusCode::GONE, "an environment is chosen per team now: PUT /v1/me/environments/{team}").into_response()
}
