//! `/v1/bench` — a person's own bench in one team: create, start, stop, the tunnel token that
//! wakes a sleeping one, and attach. Answered ONLY to `spec.owner`: there is no superadmin arm
//! here and no admin route anywhere reads a bench, because a bench is a person's transcripts.
//!
//! Every rule lives in `my_bench`, so no handler can forget one: the team is the caller's handle
//! when absent (decision 4); a non-member gets the same 404 as a stranger, a paused
//! member a 403 naming the pause, and `spec.access` is written only by the keys beat; the region is read from the
//! directory on every call and never stored on the bench (decision 1). Waking is a `wakeAt`
//! patch — `/v1` is spec's only writer, and the agent compares it to `status.idleSince`
//! (decision 6).
//!
//! There is NO delete verb here, deliberately: nobody deletes their own bench, and the only thing
//! that deletes one is the cluster controller's GC after a member removal's grace (`api::membership`,
//! `bins/controller/src/gc.rs`). What that delete takes: the chat transcripts and sessions live in
//! `.bench` inside the bench's own volume (`k8s::BENCH_SUBDIR`), so they go with the Workspace for
//! every delete reason. The user-facing place that says so in plain words is the team removal
//! confirm (`web/apps/web/src/lib/team-removal.ts`); a delete route added here would have to say
//! the same thing.
//!
//! A bench IS a Workspace (`spec.bench: Some`, `crd::is_bench` the only predicate): the routes here
//! are a FACADE over the ordinary workspace machinery, kept byte-compatible for the shipped desktop
//! and `kl-connect`. Nothing here writes a second kind, and the create goes through
//! `workspaces::allocate_and_create` — the same quota gate, key install and builder every
//! `POST /v1/workspaces` runs — so a bench can never drift into its own half-maintained path.
//!
//! The bench CONTAINER's image is not a spec field: the agent stamps it from the release's pinned
//! value, so a rolled agent moves every bench with no patch from here. `spec.image` is the
//! workspace container's, exactly as on any other workspace.

use super::scope::may_allocate_for;
use super::workspaces::{allocate_and_create, clamp_quota, gateway_url, set_desired};
use super::{caller, check_region, guard_alloc, kube, kube_err, ApiState, Caller};
use crate::crd::{self, Access, DesiredState, Phase};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use kube::api::{Api, Patch, PatchParams};
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

/// Unchanged in shape from the Bench-kind days — the desktop and `kl-connect` read these exact
/// keys — just read off the Workspace: `model` from `spec.bench`, `access` from `spec.access`.
fn bench_doc(w: &crd::Workspace, region: &str) -> serde_json::Value {
    let st = w.status.clone().unwrap_or_default();
    json!({
        "id": w.metadata.name,
        "owner": w.spec.owner,
        "team": crd::space_slug(&w.spec.owner, &w.spec.team),
        "region": region,
        "model": w.spec.bench.as_ref().map(|b| b.model.clone()).unwrap_or_default(),
        "desiredState": w.spec.desired_state,
        "access": w.spec.access,
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
) -> Result<(Caller, String, String, Option<crd::Workspace>), Response> {
    let caller = caller(s, headers).await?;
    let team = team.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).unwrap_or_else(|| caller.name.clone());
    if !super::scope::in_scope(&caller, &team) {
        return Err(super::scope::scope_refusal(&caller));
    }
    let api: Api<crd::Workspace> = Api::all(kube(s)?.clone());
    // `is_bench`, not the id shape: `bench_id` is deterministic, but an object that landed under
    // that name without being one (a restore, a hand edit) must not be served as the person's bench.
    let bench = api
        .get_opt(&crd::bench_id(&caller.name, &team))
        .await
        .map_err(kube_err)?
        .filter(|w| crd::is_bench(w) && w.spec.owner == caller.name);
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

/// The spec patch that starts or wakes a bench: `spec.bench.wakeAt`, plus `desiredState` when it
/// is a start. A MERGE patch, so `spec.bench.model` survives it — nothing else here writes spec.
fn wake_patch(running: bool, at: &str) -> serde_json::Value {
    let mut spec = json!({"bench": {"wakeAt": at}});
    if running {
        spec["desiredState"] = json!(DesiredState::Running);
    }
    json!({"spec": spec})
}

/// What waking or starting an existing bench costs: cpu and memory only. Its disk is charged from
/// the moment the volume exists (`quota::usage`), so a wake must not charge it a second time.
/// Both containers, through the one definition `quota::usage` and `k8s` also read.
fn wake_cost(res: &crd::PodResources) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::Dim;
    let (millis, mib) = crate::model::bench_pod_capacity(res);
    vec![(Dim::Cpu, millis.div_ceil(1000)), (Dim::MemoryGb, mib.div_ceil(1024))]
}

fn bench_api(s: &ApiState) -> Result<Api<crd::Workspace>, Response> {
    Ok(Api::all(kube(s)?.clone()))
}

fn found(b: Option<crd::Workspace>) -> Result<crd::Workspace, Response> {
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

/// The graft point for a recreated bench: `CloneOf{volume: id, commit}` naming the newest Ready
/// snapshot on the leftover Volume — exactly what `POST /v1/workspaces/restore` writes, which is
/// what re-attaches a working copy to a detached volume.
///
/// With no such snapshot the Volume is garbage the GC would take anyway (no worktree, nothing to
/// keep it alive), and it is deleted so the create starts from nothing. The snapshots are listed
/// UNFILTERED and matched on `spec.volume`: a label is a view, and a delete decided from a label
/// selector that missed a row would take a volume somebody's push still holds.
async fn surviving_volume(s: &ApiState, id: &str) -> Result<Option<crd::VolumeSource>, Response> {
    let c = kube(s)?.clone();
    let vols: Api<crd::Volume> = Api::all(c.clone());
    if vols.get_opt(id).await.map_err(kube_err)?.is_none() {
        return Ok(None);
    }
    let snaps: Api<crd::Snapshot> = Api::all(c);
    let newest = snaps
        .list(&Default::default())
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|sn| {
            sn.spec.volume == id
                && sn.is_snapshot()
                && sn.status.as_ref().is_some_and(|st| st.phase == Phase::Ready)
        })
        // Nameless is unreachable from an API server, and dropping it here is what keeps `commit`
        // a `Some`: `CloneOf{commit: None}` means "copy bytes into a FRESH child volume", which is
        // the one thing this must never write when a volume of that id already exists.
        .filter_map(|sn| sn.metadata.name.clone().map(|n| (sn.metadata.creation_timestamp.clone(), n)))
        .max();
    let Some((_, commit)) = newest else {
        tracing::info!(volume = %id, "bench.volume.leftover.deleted");
        vols.delete(id, &Default::default()).await.map_err(kube_err)?;
        return Ok(None);
    };
    tracing::info!(volume = %id, snapshot = %commit, "bench.volume.restored");
    Ok(Some(crd::VolumeSource::CloneOf { volume: id.to_string(), commit: Some(commit) }))
}

pub(crate) async fn create_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(body): Json<NewBench>,
) -> Result<Response, Response> {
    let (caller, team, region, existing) =
        my_bench(&s, &headers, body.team.as_deref(), body.region.as_deref()).await?;
    let api = bench_api(&s)?;
    if let Some(w) = existing {
        // Re-POSTing a stopped or idle bench starts a pod: an allocation, exactly as `start_bench`.
        if !crd::wants_pod(&w) {
            guard_alloc(&s, &caller.name, false, &wake_cost(&w.spec.resources)).await?;
        }
        let name = w.metadata.name.clone().unwrap_or_default();
        let w = api
            .patch(&name, &PatchParams::default(), &Patch::Merge(&wake_patch(true, &now())))
            .await
            .map_err(kube_err)?;
        return Ok(Json(bench_doc(&w, &region)).into_response());
    }
    let id = crd::bench_id(&caller.name, &team);
    // A bench id is DETERMINISTIC, so a recreate lands on the volume the last one left behind: a
    // deleted workspace detaches its Volume but the Volume survives whenever a pushed snapshot
    // still references it (the ordinary workspace rule). Creating fresh onto it parked the bench
    // in `Ready=False/HeadUnknown` forever — snapshots, no head — so the recreate takes the
    // RESTORE shape instead and the person's bench comes back with its data.
    let source = surviving_volume(&s, &id).await?;
    // No `refuse_taken_name`: the name is ours, the id is derived from (owner, team), and the
    // person never typed either — a workspace of theirs that happens to be called "bench" is not
    // a reason to refuse them the bench the desktop is asking for.
    let w = allocate_and_create(
        &s,
        &caller,
        &id,
        crd::WorkspaceSpec {
            // A clone, a restore and a bench all start with no trees: a tree is a nested subvolume
            // that `btrfs send` never carried, so there is nothing for a new object to inherit.
            trees: Vec::new(),
            bench: Some(crd::BenchOptions {
                model: body.model.filter(|m| !m.is_empty()).unwrap_or_else(|| crate::model::DEFAULT_BENCH_MODEL.to_string()),
                wake_at: None,
            }),
            access: Access::Full,
            owner: caller.name.clone(),
            // A PERSONAL bench carries `""` exactly as every personal workspace does. The handle
            // in `spec.team` folds to the same namespace, but the agent's `OwnerBinding` pass
            // reads a non-empty team as "this is a TEAM namespace" and would size the person's
            // `ResourceQuota` from the team defaults (~3.7x their own).
            team: if team.eq_ignore_ascii_case(&caller.name) { String::new() } else { team.clone() },
            name: BENCH_WS_NAME.to_string(),
            region: region.clone(),
            image: crate::model::default_ws_image(),
            storage: Some(crd::WorkspaceStorage { quota_gb: clamp_quota(&s, crd::BENCH_QUOTA_GB), source }),
            desired_state: DesiredState::Running,
            resources: Default::default(),
            packages: Vec::new(),
            locks: Vec::new(),
            attached_environment: None,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(bench_doc(&w, &region))).into_response())
}


/// `spec.name` of every bench. Never shown — no listing carries a bench — but it is what the
/// namespace's objects and any log line call it, so it is a word rather than an id.
const BENCH_WS_NAME: &str = "bench";


pub(crate) async fn start_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (caller, _, _, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    let b = found(b)?;
    let api = bench_api(&s)?;
    if !crd::wants_pod(&b) {
        guard_alloc(&s, &caller.name, false, &wake_cost(&b.spec.resources)).await?;
    }
    api.patch(&b.metadata.name.clone().unwrap_or_default(), &PatchParams::default(), &Patch::Merge(&wake_patch(true, &now())))
        .await
        .map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

pub(crate) async fn stop_bench(
    State(s): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(q): Query<TeamQuery>,
) -> Result<Response, Response> {
    let (_, _, _, b) = my_bench(&s, &headers, q.team.as_deref(), None).await?;
    stop_bench_ws(&s, &found(b)?).await
}


/// The one stop a bench gets, from `/v1/bench/stop` and from `/v1/workspaces/{id}/stop` alike
/// (`workspaces::stop_as` routes a bench here): the desired state, then the tool-token Secret.
/// The caller has already been authorized — `my_bench` on one route, `my_ws` on the other.
pub(crate) async fn stop_bench_ws(s: &ApiState, w: &crd::Workspace) -> Result<Response, Response> {
    let (owner, team) = (w.spec.owner.clone(), w.spec.team.clone());
    set_desired::<crd::Workspace>(kube(s)?, &w.metadata.name.clone().unwrap_or_default(), DesiredState::Stopped).await?;
    // Best effort: `caller` already refuses a stopped bench's tool token, so a leftover Secret
    // holds a dead credential until its 15 minutes run out.
    if let Err(e) = delete_tool_secret(kube(s)?, &owner, &team).await {
        tracing::warn!(%owner, %team, kind = %kube_kind(&e), "bench.tool_token.delete.failed");
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
    if b.spec.access == Access::Paused {
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
    if b.spec.access == Access::Paused {
        return Err(paused(&team));
    }
    if b.spec.desired_state == DesiredState::Stopped {
        return Err(err(StatusCode::CONFLICT, "bench is stopped; start it"));
    }
    let id = b.metadata.name.clone().unwrap_or_default();
    let phase = b.status.as_ref().map(|st| st.phase).unwrap_or_default();
    if phase == Phase::Idle {
        // Waking re-charges cpu and memory, so it is an allocation like any start.
        guard_alloc(&s, &caller.name, false, &wake_cost(&b.spec.resources)).await?;
        api.patch(&id, &PatchParams::default(), &Patch::Merge(&wake_patch(false, &now())))
            .await
            .map_err(kube_err)?;
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": "waking"}))).into_response());
    }
    if phase != Phase::Ready {
        return Ok((StatusCode::ACCEPTED, Json(json!({"state": phase.as_str()}))).into_response());
    }
    // Only a Ready=True condition the reconciler wrote proves the pod being dialled serves. The
    // reason is the WORKSPACE reconciler's (`Converged`) now that a bench is a Workspace — the
    // legacy bench reconciler's `Running` is gone, and pinning it here answered 202 forever to a
    // ready bench (owner, 2026-09-17 03:48 IST: "it didn't start yet").
    let serving = b.status.as_ref().is_some_and(|st| st.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
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

    /// A wake is `spec.bench.wakeAt` and nothing else; a start adds `desiredState`. It must stay a
    /// MERGE patch of the `bench` sub-object, or every wake would blank `spec.bench.model`.
    #[test]
    fn a_wake_writes_only_wake_at_and_a_start_adds_the_desired_state() {
        let p = wake_patch(true, "t");
        assert_eq!(p, json!({"spec": {"bench": {"wakeAt": "t"}, "desiredState": "running"}}));
        let p = wake_patch(false, "t");
        assert_eq!(p, json!({"spec": {"bench": {"wakeAt": "t"}}}));
        assert!(p["spec"]["bench"].get("model").is_none(), "a merge patch keeps the model it does not name");
    }

    /// The doc the desktop parses: the same keys as the retired Bench kind, off a Workspace.
    #[test]
    fn the_bench_doc_reads_the_model_and_access_off_the_workspace() {
        let mut w: crd::Workspace = serde_json::from_value(json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace", "metadata": {"name": "bench-abc"},
            "spec": {"owner": "alice", "team": "acme", "name": "bench", "region": "r1", "image": "i",
                     "desiredState": "running", "access": "paused", "bench": {"model": "m/1"}},
        }))
        .unwrap();
        w.status = Some(crd::WorkspaceStatus { phase: Phase::Ready, node_name: "node-a".into(), ..Default::default() });
        let d = bench_doc(&w, "r1");
        assert_eq!(
            d,
            json!({"id": "bench-abc", "owner": "alice", "team": "acme", "region": "r1", "model": "m/1",
                   "desiredState": "running", "access": "paused", "phase": "ready", "nodeName": "node-a",
                   "conditions": []})
        );
    }
}
