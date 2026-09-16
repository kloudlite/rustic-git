//! `/v1/bench` — a person's own bench in one team: create, start, stop, the tunnel token that
//! wakes a sleeping one, and attach. Answered ONLY to `spec.owner`: there is no superadmin arm
//! here and no admin route anywhere reads a bench, because a bench is a person's transcripts.
//!
//! Every rule lives in `my_bench`, so no handler can forget one: the team is the caller's handle
//! when absent (decision 4); a non-member gets the same 404 as a stranger, a paused
//! member a 403 naming the pause, and `spec.access` is written only by the keys beat; the region is read from the
//! directory on every call and never stored on the Bench (decision 1). Waking is a `wakeAt`
//! patch — `/v1` is spec's only writer, and the agent compares it to `status.idleSince`
//! (decision 6).
//!
//! There is NO delete verb here, deliberately: nobody deletes their own bench, and the only thing
//! that deletes one is the cluster controller's GC after a member removal's grace (`api::membership`,
//! `bins/controller/src/gc.rs`). What that delete takes belongs beside the create that stamps it:
//! the Bench carries `BENCH_FOLDER_FINALIZER`, so the bench folder `.benches/{team}/{owner}` — the
//! chat transcripts and sessions — is deleted with the Bench, for every delete reason
//! (`bins/agent/src/controller/bench.rs`). The user-facing place that says so in plain words is the
//! team removal confirm (`web/apps/web/src/lib/team-removal.ts`); a delete route added here would
//! have to say the same thing.
//!
//! Image upgrades: `spec.image` tracks `KLOUDLITE_BENCH_IMAGE` (pinned per release). Every patch
//! that starts or wakes a bench (`wake_patch`) re-stamps it when it differs, so a stopped or idle
//! bench starts on the new image. A RUNNING pod is never replaced for an image change — the agent
//! never compares images — so a session is never killed mid-turn; the pod exits on its own idle
//! clock and the next wake creates it from the new `spec.image`.

use super::scope::may_allocate_for;
use super::workspaces::{gateway_url, install_user_key_when, set_desired};
use super::{bench_cost, caller, check_region, guard_alloc, kube, kube_err, ApiState, Caller};
use crate::crd::{self, BenchAccess, DesiredState, Phase};
use crate::k8s::{labels, TEAM_LABEL};
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

/// caller, normalized team, region, and the caller's own bench — a paused member's 403, or the
/// 404 every other non-member gets. `first` is only a create's body region (a person's first bind).
async fn my_bench(
    s: &ApiState,
    headers: &HeaderMap,
    team: Option<&str>,
    first: Option<&str>,
) -> Result<(Caller, String, String, Option<crd::Bench>), Response> {
    let caller = caller(s, headers).await?;
    let team = team.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).unwrap_or_else(|| caller.name.clone());
    if !super::scope::in_scope(&caller, &team) {
        return Err(super::scope::scope_refusal(&caller));
    }
    let api: Api<crd::Bench> = Api::all(kube(s)?.clone());
    let bench = api
        .get_opt(&crd::bench_id(&caller.name, &team))
        .await
        .map_err(kube_err)?
        .filter(|b| b.spec.owner == caller.name);
    if !may_allocate_for(s, &caller, &team).await {
        // A paused member, or a removed one who still has a bench here, is told so; anyone else
        // gets the stranger's 404.
        return Err(match super::scope::team_access(s, &caller, &caller.name, &team, bench.is_some()).await {
            Err((st, msg)) => err(st, msg),
            Ok(()) => no_team(),
        });
    }
    let region = team_region(s, &caller, &team, first).await?;
    Ok((caller, team, region, bench))
}

fn paused(team: &str) -> Response {
    err(StatusCode::FORBIDDEN, format!("your access to {team} is paused"))
}

fn configured_image() -> String {
    std::env::var("KLOUDLITE_BENCH_IMAGE")
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| crate::model::DEFAULT_BENCH_IMAGE.to_string())
}

/// The spec patch that starts or wakes `b`; carries `image` only when it moved, so an unchanged
/// bench's patch stays as small as it was.
fn wake_patch(b: &crd::Bench, image: &str, running: bool, at: &str) -> serde_json::Value {
    let mut spec = json!({"wakeAt": at});
    if running {
        spec["desiredState"] = json!(DesiredState::Running);
    }
    if b.spec.image != image {
        spec["image"] = json!(image);
    }
    json!({"spec": spec})
}

fn bench_api(s: &ApiState) -> Result<Api<crd::Bench>, Response> {
    Ok(Api::all(kube(s)?.clone()))
}

fn found(b: Option<crd::Bench>) -> Result<crd::Bench, Response> {
    b.ok_or_else(|| err(StatusCode::NOT_FOUND, "no bench"))
}

/// The spaces a bench may be opened in, for the desktop picker: `[{slug, name, region, personal}]` —
/// first the caller's own personal space (slug = their handle, which is exactly the `team` every
/// bench route already treats as personal), then the teams the TOKEN's person is a current member
/// of ("" region = unbound), and nothing else — no
/// members, roles or quotas. Identity is only ever the verified bearer (`caller`, which checks a
/// CLI login's revocation); nothing in the query or headers names a user. Membership is read
/// uncached, so a removed member loses the row on the next call; an unreadable directory is a 503
/// with no detail, never an empty list. `no-store` on every answer.
pub(crate) async fn bench_teams(State(s): State<Arc<ApiState>>, headers: HeaderMap) -> Response {
    let mut r = list_bench_teams(&s, &headers).await.unwrap_or_else(|e| e);
    r.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    r
}

async fn list_bench_teams(s: &ApiState, headers: &HeaderMap) -> Result<Response, Response> {
    let caller = caller(s, headers).await?;
    let unavailable = |e: String| {
        tracing::error!(caller = %caller.name, error = %e, "bench.teams.directory.failed");
        err(StatusCode::SERVICE_UNAVAILABLE, "team list unavailable")
    };
    let dir = s.directory.as_ref().ok_or_else(|| unavailable("no directory".into()))?;
    let region = dir.personal_region(&caller.name).await.map_err(&unavailable)?;
    let mut out = vec![json!({"slug": caller.name, "name": "Personal", "region": region, "personal": true})];
    for slug in dir.member_teams(&caller.name).await.map_err(&unavailable)? {
        // A team deleted between the two reads is simply not listed.
        if let Some((name, region)) = dir.bench_team(&slug).await.map_err(&unavailable)? {
            out.push(json!({"slug": slug, "name": name, "region": region, "personal": false}));
        }
    }
    tracing::info!(caller = %caller.name, count = out.len(), "bench.teams.listed");
    Ok(Json(out).into_response())
}

pub(crate) async fn get_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (_, _, region, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let b = found(b)?;
    Ok(Json(bench_doc(&b, &region)).into_response())
}

pub(crate) async fn create_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(body): Json<NewBench>,
) -> Result<Response, Response> {
    let (caller, team, region, existing) =
        my_bench(&s, &headers, body.team.as_deref(), body.region.as_deref()).await?;
    let api = bench_api(&s)?;
    if let Some(b) = existing {
        // Re-POSTing a stopped or idle bench starts a pod: an allocation, exactly as `start_bench`.
        if !crd::bench_wants_pod(&b) {
            guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
        }
        let name = b.metadata.name.clone().unwrap_or_default();
        let patch = wake_patch(&b, &configured_image(), true, &now());
        let b = api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
        return Ok(Json(bench_doc(&b, &region)).into_response());
    }
    guard_alloc(&s, &caller.name, false, &bench_cost(&crd::PodResources::default())).await?;
    let id = crd::bench_id(&caller.name, &team);
    let image = configured_image();
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
    b.metadata.finalizers = Some(vec![crd::BENCH_FOLDER_FINALIZER.to_string()]);
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
    let (caller, _, _, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let b = found(b)?;
    let api = bench_api(&s)?;
    if !crd::bench_wants_pod(&b) {
        guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
    }
    let patch = wake_patch(&b, &configured_image(), true, &now());
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
    let (caller, team, _, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let b = found(b)?;
    set_desired::<crd::Bench>(kube(&s)?, &b.metadata.name.clone().unwrap_or_default(), DesiredState::Stopped).await?;
    // Best effort: `caller` already refuses a stopped bench's tool token, so a leftover Secret
    // holds a dead credential until its 15 minutes run out.
    if let Err(e) = delete_tool_secret(kube(&s)?, &caller.name, &team).await {
        tracing::warn!(owner = %caller.name, %team, kind = %kube_kind(&e), "bench.tool_token.delete.failed");
    }
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Delete the bench's tool-token Secret; already gone counts as done.
pub(crate) async fn delete_tool_secret(c: &kube::Client, owner: &str, team: &str) -> Result<(), kube::Error> {
    let api: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(c.clone(), &crd::ws_namespace(owner, team));
    match api.delete(crate::k8s::BENCH_TOOL_SECRET, &Default::default()).await {
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        r => r.map(|_| ()),
    }
}

/// The error's kind, never its text: an apply error can quote the request body, which holds the token.
fn kube_kind(e: &kube::Error) -> String {
    match e {
        kube::Error::Api(ae) => format!("api {}", ae.code),
        _ => "transport".into(),
    }
}

/// The desktop app mints the bench pod a 15-minute platform token, written where only that pod
/// reads it. A CLI login only: a session cookie has no parent to die with (403), and a bench-tool
/// token never reaches here (`caller` 401s it), so a pod cannot extend itself.
pub(crate) async fn mint_tool_token(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, team, _, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let Some(parent) = caller.parent.clone() else {
        return Err(err(StatusCode::FORBIDDEN, "sign in on the Kloudlite desktop app"));
    };
    let b = found(b)?;
    // A member whose beat has not caught up still holds a Paused bench: no tools either way.
    if b.spec.access == BenchAccess::Paused {
        return Err(paused(&team));
    }
    if b.spec.desired_state == DesiredState::Stopped {
        return Err(err(StatusCode::CONFLICT, "bench is stopped; start it"));
    }
    let id = b.metadata.name.clone().unwrap_or_default();
    let (token, claims) = s.jwt.mint_bench_tool(&caller.name, &team, &id, &parent).map_err(|e| {
        tracing::error!(error = %e, "bench.tool_token.mint.failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, "could not mint a tool token")
    })?;
    let ns = crd::ws_namespace(&caller.name, &team);
    let api: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(kube(&s)?.clone(), &ns);
    let secret = crate::k8s::bench_tool_secret(&ns, &token, claims.exp);
    if let Err(e) = api
        .patch(crate::k8s::BENCH_TOOL_SECRET, &PatchParams::apply("kloudlite-api").force(), &Patch::Apply(&secret))
        .await
    {
        tracing::warn!(owner = %caller.name, %team, kind = %kube_kind(&e), "bench.tool_token.write.failed");
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "could not write the bench tool token"));
    }
    let short = |x: &str| x.chars().take(8).collect::<String>();
    tracing::info!(owner = %caller.name, %team, jti8 = %short(&claims.jti), parent8 = %short(&parent), exp = claims.exp, "bench.tool_token.written");
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn revoke_tool_token(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, team, _, _) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    delete_tool_secret(kube(&s)?, &caller.name, &team).await.map_err(|e| {
        tracing::warn!(owner = %caller.name, %team, kind = %kube_kind(&e), "bench.tool_token.delete.failed");
        err(StatusCode::SERVICE_UNAVAILABLE, "could not delete the bench tool token")
    })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Every tunnel connection asks here; an idle bench is woken and the client re-asks until 201.
pub(crate) async fn bench_session(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, team, region, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let b = found(b)?;
    let api = bench_api(&s)?;
    if b.spec.access == BenchAccess::Paused {
        return Err(paused(&team));
    }
    if b.spec.desired_state == DesiredState::Stopped {
        return Err(err(StatusCode::CONFLICT, "bench is stopped; start it"));
    }
    let id = b.metadata.name.clone().unwrap_or_default();
    let phase = b.status.as_ref().map(|st| st.phase).unwrap_or_default();
    if phase == Phase::Idle {
        // Waking re-charges cpu and memory, so it is an allocation like any start.
        guard_alloc(&s, &caller.name, false, &bench_cost(&b.spec.resources)).await?;
        api.patch(&id, &PatchParams::default(), &Patch::Merge(&wake_patch(&b, &configured_image(), false, &now())))
            .await
            .map_err(kube_err)?;
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": "waking"}))).into_response());
    }
    if phase != Phase::Ready {
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": phase.as_str()}))).into_response());
    }
    // Only a Ready condition the reconciler wrote (`Running`) proves the pod being dialled serves.
    let serving = b.status.as_ref().is_some_and(|st| st.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True" && c.reason == "Running"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_on_an_old_image_moves_spec_image_and_an_unchanged_one_does_not() {
        let b = crd::Bench::new(
            "bench-1",
            serde_json::from_value(json!({"owner": "alice", "team": "acme", "image": "bench:old", "desiredState": "stopped"})).unwrap(),
        );
        let p = wake_patch(&b, "bench:new", true, "t");
        assert_eq!(p["spec"]["image"], "bench:new");
        assert_eq!(p["spec"]["desiredState"], "running");
        let p = wake_patch(&b, "bench:old", false, "t");
        assert!(p["spec"].get("image").is_none() && p["spec"].get("desiredState").is_none());
        assert_eq!(p["spec"]["wakeAt"], "t");
    }
}
