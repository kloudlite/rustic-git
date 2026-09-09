//! `/v1/workspaces` — create, list, read, delete, start/stop, attach/detach, package edits,
//! clone and restore-to-new, plus the ssh connect ticket and the owner's platform key install.

use super::scope::{find_env, may_act_on, may_allocate_for, mine, my_ws, owned_by, owned_in, refuse_taken_name};
use super::{caller, check_region, guard_alloc, is_missing, kube, kube_err, not_found, not_ready, phase, rid, workspace_cost, ApiState};
use super::push::{clone_base, with_based_on};
use super::volumes::{find_snapshot, volume_region};
use crate::crd::{self, DesiredState, VolumeSource};
use crate::k8s::{labels, ATTACHED_ENV_LABEL, TEAM_LABEL};
use crate::model::*;
use crate::packages::resolve::Refusal;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Resource, ResourceExt};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::collections::HashSet;
use std::sync::Arc;

mod keys;
pub use keys::*;
mod ssh;
pub(crate) use ssh::*;
mod attach;
pub(crate) use attach::*;
mod packages;
pub(crate) use packages::*;
mod clone_restore;
pub(crate) use clone_restore::*;


/// The child `Volume`'s name, from STATUS alone: the reconciler creates the Volume and then
/// reports it, so that is the fact.
pub(crate) fn ws_volume(w: &crd::Workspace) -> Option<&str> {
    w.status.as_ref().and_then(|st| st.volume_ref.as_deref()).filter(|v| !v.is_empty())
}


/// Every volume of `owner` that has ever landed a snapshot (`spec.transient: false` — a sync
/// point never makes a workspace/environment doc's `volume` field non-null).
///
/// Answered from the `Snapshot` CRs themselves, label-selected then re-checked against
/// `spec.owner` (`mine`, never the label). It is a QUERY rather than a Volume status field
/// because a field would need a second controller writing the Volume's status — `patch_status`
/// force-applies under one field manager, so the Volume reconciler's next pass would prune it.
///
/// ONE call per REQUEST, passed down to every row: one lookup per row turns a listing into an N+1.
pub(crate) async fn pushed_volumes(_s: &ApiState, c: &kube::Client, owner: &str) -> Result<HashSet<String>, Response> {
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let items = mine(api.list(&owned_by(owner)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner.to_string()));
    // Any phase but Error, on purpose: the same predicate the finalizer uses to decide a snapshot
    // still references the volume, so a push that is still uploading already shows the volume it
    // will keep alive. The old registry path answered only after the upload landed.
    Ok(items
        .into_iter()
        .filter(|s| s.is_snapshot() && s.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error))
        .map(|s| s.spec.volume)
        .collect())
}


pub(super) fn ws_doc(w: &crd::Workspace, pushed: &HashSet<String>) -> Workspace {
    let id = w.name_any();
    let st = w.status.as_ref();
    let seed = match w.spec.storage.as_ref().and_then(|s| s.source.as_ref()) {
        Some(crd::VolumeSource::GitRepo { repo, branch }) => Some((repo.clone(), branch.clone())),
        _ => None,
    };
    Workspace {
        owner: w.spec.owner.clone(),
        team: w.spec.team.clone(),
        name: w.spec.name.clone(),
        region: w.spec.region.clone(),
        state: phase(st.map(|s| s.phase.as_str()), WsState::Creating),
        image: w.spec.image.clone(),
        // `None` until a node claims it — the web renders that as "not placed yet" rather than as
        // a node that was never true.
        placement: st.map(|s| s.node_name.clone()).filter(|n| !n.is_empty()),
        volume: ws_volume(w)
            .filter(|v| pushed.contains(*v))
            .map(|_| format!("vol/{}/{id}", w.spec.owner)),
        quota_gb: w.spec.storage.as_ref().map(|s| s.quota_gb).unwrap_or(0),
        packages: w.spec.packages.clone(),
        base_packages: st.and_then(|s| s.packages.as_ref()).map(|p| p.base.clone()).unwrap_or_default(),
        // Filled in only once the pod has reported a host key: the web's ssh snippet is the same
        // pair the CLI gets from a mint, so the page needs no token to show the command.
        ssh: st.and_then(|s| s.ssh_host_key.clone()).map(|host_key| SshDoc {
            gateway: gateway_url(&w.spec.region, &id),
            host_key,
        }),
        packages_status: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == crd::PACKAGES_READY).map(ConditionDoc::from)),
        replicated: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Replicated").map(ConditionDoc::from)),
        degraded: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Degraded").map(ConditionDoc::from)),
        decommissioning: st.and_then(|s| s.conditions.iter().find(|c| c.type_ == "Decommissioning").map(ConditionDoc::from)),
        placed: st
            .and_then(|s| s.conditions.iter().find(|c| c.type_ == "Placed"))
            .filter(|c| c.status != "True" && c.reason == "NoCapacity")
            .map(ConditionDoc::from),
        locks: w.spec.locks.iter().map(LockDoc::from).collect(),
        repo: seed.as_ref().map(|(r, _)| r.clone()),
        branch: seed.map(|(_, b)| b),
        attached_environment: crd::attached_environment(w),
        id,
    }
}


/// Flip `spec.desiredState`. A merge patch, not an apply: this touches one field and must not
/// claim ownership of the rest of a spec the caller never sent.
pub(crate) async fn set_desired<K>(c: &kube::Client, id: &str, want: DesiredState) -> Result<(), Response>
where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
{
    let api: Api<K> = Api::all(c.clone());
    let patch = serde_json::json!({"spec": {"desiredState": want}});
    api.patch(id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(())
}

// ── workspaces ───────────────────────────────────────────────────────────


#[derive(serde::Deserialize)]
pub(crate) struct NewWorkspace {
    /// The team to make it in. Absent, or the caller's own handle, means personal.
    #[serde(default)]
    team: Option<String>,
    name: String,
    region: String,
    quota_gb: u64,
    #[serde(default = "default_ws_image")]
    image: String,
    /// Seed the workspace from a PLATFORM repository, as `owner/name`. Not a URL, deliberately:
    /// a URL here would be an egress and SSRF primitive available to anyone who can create a
    /// workspace, and nothing off this platform is in the trust boundary anyway.
    #[serde(default)]
    repo: Option<String>,
    /// The branch to start from. Required with `repo` — "whatever the default is" is a different
    /// workspace depending on when it was created.
    #[serde(default)]
    branch: Option<String>,
    /// nixpkgs attribute names to install into the workspace's profile.
    #[serde(default)]
    packages: Vec<String>,
}


/// 422, not 400: the body parsed fine, one of its values is unusable — and the web shows this
/// string to the caller who typed the name.
pub(super) fn bad_packages(e: crate::packages::PackageError) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": e.to_string()}))).into_response()
}


/// The one place a `Refusal` becomes a status. `Malformed` is a caller bug (the entry never
/// passed `validate_list`), `Unknown` is the person's typo, `Unavailable` is ours.
pub(super) fn refuse(r: Refusal) -> Response {
    let code = match r {
        Refusal::Malformed(_) => StatusCode::BAD_REQUEST,
        Refusal::Unknown { .. } | Refusal::NotCached { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        Refusal::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    (code, Json(serde_json::json!({"error": r.to_string()}))).into_response()
}


/// Locks for `packages`, run BEFORE the quota gate and the CR write in every handler that writes
/// a package list — a refusal must leave nothing behind.
///
/// A list with no `@` entry never touches the resolver, which is what keeps a dev deployment with
/// no index configured working exactly as it did before pins existed.
pub(super) async fn lock_for(
    s: &ApiState,
    packages: &[String],
    prev: &[crd::Lock],
    refresh: bool,
) -> Result<Vec<crd::Lock>, Response> {
    if !packages.iter().any(|p| p.contains('@')) {
        return Ok(Vec::new());
    }
    let Some(r) = s.resolver.as_ref() else {
        return Err(refuse(Refusal::Unavailable));
    };
    r.lock_all(packages, prev, refresh).await.map_err(refuse)
}


/// The one gate on a workspace or environment name, on every route that accepts one. The name ends up verbatim
/// in generated ssh config on a TEAMMATE's machine (`model::valid_ws_name`), so it is checked
/// where it enters the system rather than at each renderer — the renderers refuse too, but a
/// stored bad name would already have made every listing of that team unusable.
pub(crate) fn check_ws_name(name: &str) -> Result<(), Response> {
    if valid_ws_name(name) {
        return Ok(());
    }
    Err((
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({
            "error": "name must be 1-63 characters of letters, digits, '.', '_' or '-'"
        })),
    )
        .into_response())
}


/// `0` is a qgroup nothing can start on; the upper end is `settings.quota_gb_ceiling`, live
/// per-region rather than a compiled-in number. Clamped rather than refused: the web sends a
/// fixed default, and a client that asks for more than the ceiling gets the ceiling.
pub(crate) fn clamp_quota(s: &ApiState, gb: u64) -> u64 {
    gb.clamp(1, s.settings.load().quota_gb_ceiling as u64)
}


pub(crate) async fn create_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewWorkspace>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    check_ws_name(&body.name)?;
    check_region(&s, &body.region).await?;
    let team = match body.team.as_deref().map(str::trim).filter(|t| !t.is_empty() && *t != owner.name) {
        None => String::new(),
        // Lowercased BEFORE `may_allocate_for`: the directory's team slugs are lowercase, so a check
        // on the raw casing 404'd a real member of `acme` who typed `Acme`. 404, not 403, on a miss:
        // whether a team exists is not a non-member's to learn, same as every other owner-scoped route.
        // `may_allocate_for`, not `may_act_on`: this NAMES the new workspace's billed owner, and a
        // superadmin claim must never spend a team's quota without being a member.
        Some(t) => {
            let t = t.to_lowercase();
            if may_allocate_for(&s, &owner, &t).await {
                t
            } else {
                return Err((StatusCode::NOT_FOUND, "no such team").into_response());
            }
        }
    };
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    let locks = lock_for(&s, &body.packages, &[], false).await?;
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;
    let quota_gb = clamp_quota(&s, body.quota_gb);
    // The object's owner is the team when one is given — a team's workspaces count against the
    // team, never against whoever happened to click create.
    let owner_of = if team.is_empty() { owner.name.clone() } else { team.clone() };
    guard_alloc(&s, &owner_of, !team.is_empty(), &workspace_cost(quota_gb, &crd::PodResources::default())).await?;
    let id = rid("ws");
    let source = match (&body.repo, &body.branch) {
        (None, _) => None,
        (Some(_), None) => {
            return Err((StatusCode::BAD_REQUEST, "branch is required with repo").into_response())
        }
        (Some(repo), Some(branch)) => {
            // `owner/name`, checked here so a bad value is a 400 rather than a workspace that
            // fails later. `k8s::git_init_container` re-checks it, and that is the check that
            // matters: it is the last point before the value becomes an ssh argv, and it also
            // covers a Volume written by any path that is not this handler.
            let ok = repo
                .split_once('/')
                .is_some_and(|(o, n)| kloudlite_storage::store::valid_owner(o)
                    && kloudlite_storage::store::valid_segment(n));
            if !ok {
                return Err((StatusCode::BAD_REQUEST, "repo must be owner/name").into_response());
            }
            Some(crd::VolumeSource::GitRepo {
                repo: repo.clone(),
                branch: branch.clone(),
            })
        }
    };
    // ONE object. Placement and the child `Volume` are the controllers' — the node this lands on
    // is a fact this process has no way to know yet, and a wish about a fact is how the two ever
    // disagreed about where the data is (audit H1).
    let w = create_workspace(
        c,
        &id,
        crd::WorkspaceSpec {
            owner: owner.name.clone(),
            team: team.clone(),
            name: body.name,
            region: body.region,
            image: body.image,
            storage: Some(crd::WorkspaceStorage { quota_gb, source }),
            desired_state: DesiredState::Running,
            resources: Default::default(),
            packages: body.packages,
            locks,
            attached_environment: None,
        },
    )
    .await?;
    // Every owner builds images through their own hidden buildkit environment, and this is the
    // moment they are known to need one. Awaited, so a create's own test can see it, but best
    // effort: a builder that fails to appear costs the owner `kl build` until the next create,
    // never the workspace they actually asked for.
    if let Err(e) = super::environments::ensure_builder(&s, &owner.name, &team, &w.spec.region).await {
        tracing::warn!(owner = %owner.name, team = %team, status = ?e.status(), "builder.ensure.failed");
    }
    // Off the request: the wait is up to 5 s of polling for a node to claim the object, and the
    // 202 already says "accepted, not done". `list_ws` re-installs an absent key regardless.
    tokio::spawn({
        let (s, c, owner, team, id) = (s.clone(), c.clone(), owner.clone(), team.clone(), id.clone());
        async move { install_user_key_after_placed(&s, &c, &owner, &team, &id).await }
    });
    // The pod mounts the projected file, so a FIRST workspace whose owner has no `OwnerKeys` yet
    // would park in `KeysNotReady` until the resync beat (300 s) noticed it. Projecting here makes
    // that seconds. Fire and forget: the beat is still the guarantee, so a failure is a log line,
    // never a failed create.
    tokio::spawn({
        let (s, keys_owner) = (s.clone(), crate::k8s::keys_owner(&w.spec).to_string());
        async move {
            if let Err(e) = super::keys::project(&s, &keys_owner).await {
                tracing::warn!(owner = %keys_owner, error = %e, "keys.project.failed");
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &HashSet::new()))).into_response())
}


/// The one place a `Workspace` is written. Labels are a VIEW of `spec.owner`/`spec.team`, stamped
/// here so listings are indexed label selectors rather than scans.
pub(super) async fn create_workspace(c: &kube::Client, id: &str, spec: crd::WorkspaceSpec) -> Result<crd::Workspace, Response> {
    let mut l = labels(&spec.owner, "workspace");
    l.insert(TEAM_LABEL.to_string(), spec.team.clone());
    let mut w = crd::Workspace::new(id, spec);
    w.metadata.labels = Some(l);
    let api: Api<crd::Workspace> = Api::all(c.clone());
    api.create(&PostParams::default(), &w).await.map_err(kube_err)
}


pub(crate) async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    // `?team=` scopes the list to the caller's workspaces IN that team; absent means personal —
    // `list_for_owner` is exactly that default case, shared with `/admin`, whose ?owner= names an
    // exact owner and never a team-narrowed view of one.
    let team = match q.get("team").map(|t| t.trim()).filter(|t| !t.is_empty() && *t != owner.name) {
        None => return list_for_owner(&s, &headers, &owner.name).await,
        // Same casing fix as `create_ws`: lowercase before the membership check, not after.
        Some(t) => {
            let t = t.to_lowercase();
            if may_act_on(&s, &owner, &t).await {
                t
            } else {
                return Err((StatusCode::NOT_FOUND, "no such team").into_response());
            }
        }
    };
    let c = kube(&s)?;
    // No "filter out the deleted ones": a deleted object is gone from the API server.
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let items = mine(api.list(&owned_in(&owner, &team)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner.name));
    let pushed = pushed_volumes(&s, c, &owner).await?;
    let list: Vec<_> = items.iter().map(|w| ws_doc(w, &pushed)).collect();
    // The retry the create's 5 s ceiling defers to: cheap, idempotent, and the only place a user
    // whose very first workspace outran its namespace is ever seen again. Seeded pods REQUIRE the
    // key mount, so "it lands next time" is not good enough on its own.
    if !items.is_empty() && s.keys.is_some() {
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(c.clone(), &crd::ws_namespace(&owner, &team));
        if matches!(secrets.get_opt(crate::k8s::USER_KEY_SECRET).await, Ok(None)) {
            write_user_key(&s, c, &crd::ws_namespace(&owner, &team), &owner).await;
        }
    }
    Ok(Json(list).into_response())
}


/// The shared body of `list_ws`'s no-`?team=` (personal) branch and `/admin/workspaces`'s
/// `?owner=`: an exact owner, authorized with `may_act_on` — the caller's own always passes, a
/// team member passes for a team-owned filter (workspaces have none today, but the check costs
/// nothing to keep uniform), and a superadmin passes for anyone, logged there.
pub(crate) async fn list_for_owner(
    s: &ApiState,
    headers: &axum::http::HeaderMap,
    owner: &str,
) -> Result<Response, Response> {
    let caller_id = caller(s, headers).await?;
    if !may_act_on(s, &caller_id, owner).await {
        return Err(not_found());
    }
    let list = ws_for_owner(s, owner).await?;
    // The key-minting side effect belongs to this `/v1` wrapper, never to `ws_for_owner` itself —
    // the admin Owner detail page calls `ws_for_owner` directly to render a read-only page, and a
    // GET there must not have the side effect of writing a namespace Secret.
    if !list.is_empty() && s.keys.is_some() {
        let c = kube(s)?;
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(c.clone(), &crd::ws_namespace(owner, ""));
        if matches!(secrets.get_opt(crate::k8s::USER_KEY_SECRET).await, Ok(None)) {
            write_user_key(s, c, &crd::ws_namespace(owner, ""), owner).await;
        }
    }
    Ok(Json(list).into_response())
}


/// The read half of `list_for_owner`, owner already authorized — the admin Owner detail page
/// reuses this directly since the claim already stands in for `may_act_on` there. Read-only: no
/// Secret is written here, see `list_for_owner`'s own key-minting step.
pub(crate) async fn ws_for_owner(s: &ApiState, owner: &str) -> Result<Vec<Workspace>, Response> {
    let c = kube(s)?;
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let items = mine(api.list(&owned_in(owner, "")).await.map_err(kube_err)?.items, std::slice::from_ref(&owner.to_string()));
    let pushed = pushed_volumes(s, c, owner).await?;
    Ok(items.iter().map(|w| ws_doc(w, &pushed)).collect())
}


pub(crate) async fn get_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let pushed = pushed_volumes(&s, kube(&s)?, &owner).await?;
    Ok(Json(ws_doc(&w, &pushed)).into_response())
}


/// One apex for every region's ssh gateway; the per-region name (`ws-{region}.`) is a proxied
/// Cloudflare record pointing at that region's nodes, created when the region is stood up. A const
/// rather than config because a second domain would mean a second origin certificate, not a new
/// value to set.
pub(super) const GATEWAY_DOMAIN: &str = "khost.dev";


pub(super) fn gateway_url(region: &str, id: &str) -> String {
    format!("wss://ws-{region}.{GATEWAY_DOMAIN}/tunnel/{id}")
}


/// ONE delete. The "Workspace first, then Volume" ordering became the API server's job the moment
/// the Volume got an ownerReference: garbage collection follows it, and the Volume's own finalizer
/// still holds the reclaim until the subvolume is gone.
pub(crate) async fn delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    delete_as(&s, &headers, &id).await
}


/// Shared by `/v1/workspaces/{id}` (caller as owner, via `my_ws`) and
/// `/admin/workspaces/{id}` (any owner, `my_ws`'s superadmin arm) — one delete, one caller either
/// way, `my_ws` is what decides whether this request may touch it at all.
pub(crate) async fn delete_as(
    s: &ApiState,
    headers: &axum::http::HeaderMap,
    id: &str,
) -> Result<Response, Response> {
    let owner = caller(s, headers).await?;
    let w = my_ws(s, &owner, id).await?;
    let c = kube(s)?;
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    // Nothing stamps a finalizer on a Workspace, so its deletion is pure garbage collection and the
    // agent never observes it. The workspace-side policy goes with its ownerReference and the
    // attach directory is swept by the janitor, but the ENVIRONMENT-side half lives in another
    // namespace under the Environment's ownership — so it is removed here. The Workspace goes
    // FIRST: an agent pass landing between the two would otherwise re-`ensure` the grant and then
    // find no object left to ever remove it again.
    let env = crd::attached_environment(&w);
    // A 404 here is the desired state already reached — another caller raced us to delete the
    // same Workspace — and must fall through to collect the policy below, not short-circuit and
    // orphan it (same idea as `delete_ignoring_404` in the agent).
    if let Err(e) = ws.delete(id, &DeleteParams::default()).await {
        if !is_missing(&e) {
            return Err(kube_err(e));
        }
    }
    drop_attach_policy(c, id, env.as_deref()).await;
    let mut doc = ws_doc(&w, &HashSet::new());
    doc.state = WsState::Deleted;
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}


pub(crate) async fn start_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    if w.status.as_ref().is_some_and(|st| interrupted(&st.conditions)) {
        return Err(interrupted_409("workspace"));
    }
    set_desired::<crd::Workspace>(kube(&s)?, &id, DesiredState::Running).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}


/// The person is the one who decides whether a Running worktree pinned to a dead node is worth
/// losing (see the design's "the person decides" rule): stopping it is that decision, so the
/// response says what it costs, read off the `NodeDead` condition the sweep already wrote.
pub(crate) fn node_dead_warning(node_name: &str, conditions: &[crd::Condition]) -> Option<String> {
    interrupted(conditions)
        .then(|| format!("node {node_name} is down; edits after the last sync point are only on that node and will not follow the move"))
}


/// Interrupted: the node died while this was RUNNING, so its live edits exist only there. The
/// sweep writes `Degraded/NodeDead` and keeps the pin; nothing in the system may move it. Both the
/// type and the reason, not the reason alone — `NodeDead` is a specific enough token that nothing
/// else uses it today, but matching only half of what the sweep writes is how this and the sweep
/// drift apart the day something else reuses the reason on a different condition type.
pub(crate) fn interrupted(conditions: &[crd::Condition]) -> bool {
    conditions.iter().any(|c| c.type_ == "Degraded" && c.reason == "NodeDead" && c.status == "True")
}


/// The one answer a start gets while a parent is interrupted. There is deliberately no force
/// flag: abandoning someone's edits is not a thing this API can offer, and the way forward is a
/// clone from the last synced point — which `clone` allows, with its age stated.
pub(crate) fn interrupted_409(kind: &str) -> Response {
    (StatusCode::CONFLICT, format!("{kind} is interrupted: its node is down; it resumes when the node returns")).into_response()
}


pub(crate) async fn stop_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    stop_as(&s, &headers, &id).await
}


/// Shared by `/v1/workspaces/{id}/stop` and `/admin/workspaces/{id}/stop` — see `delete_as`.
pub(crate) async fn stop_as(
    s: &ApiState,
    headers: &axum::http::HeaderMap,
    id: &str,
) -> Result<Response, Response> {
    let owner = caller(s, headers).await?;
    let w = my_ws(s, &owner, id).await?;
    set_desired::<crd::Workspace>(kube(s)?, id, DesiredState::Stopped).await?;
    // Every non-204 success is `res.json()`'d by the web client (web/apps/web/src/lib/api.ts) —
    // a body-less 202 throws there, so this always emits an object, `warning` present only when
    // there is one to give.
    // The whole doc, not a bare `{}`: the caller needs `replicated` to know whether this may be
    // started elsewhere, and a second round trip for it would race the stop it just asked for.
    let warning = w.status.as_ref().and_then(|st| node_dead_warning(&st.node_name, &st.conditions));
    // The real pushed set, not `HashSet::new()`: an empty one made `volume` null on every mutation
    // response even for a volume with fifty pushes, and a client reading that as "never pushed" got
    // a wrong answer from all seven of these handlers. The WORKSPACE's owner, not the caller — the
    // two differ on `/admin`, and the caller's own pushed set would answer the wrong question.
    let pushed = pushed_volumes(s, kube(s)?, &w.spec.owner).await?;
    let mut doc = ws_doc(&w, &pushed);
    doc.state = WsState::Stopped;
    #[allow(clippy::expect_used)] // serde of a derive(Serialize) value of ours cannot fail
        let mut body = serde_json::to_value(&doc).expect("Workspace doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}


/// Delete the environment-side half of an attachment grant, which lives in a namespace the
/// Workspace's ownerReference cannot reach. Best-effort with a warning: the environment's own
/// deletion collects it either way, and a grant left behind is dormant until something re-adds an
/// egress with the same workspace id.
pub(super) async fn drop_attach_policy(c: &kube::Client, id: &str, env: Option<&str>) {
    let Some(env) = env else { return };
    let policies: Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
        Api::namespaced(c.clone(), &crd::env_namespace(env));
    if let Err(e) = policies.delete(&crate::k8s::attach_policy_name(id), &DeleteParams::default()).await {
        tracing::warn!(workspace = %id, environment = %env, error = %e, "attach.policy.delete.failed");
    }
}


/// What a copy of `volume` should be sized at.
///
/// A release-1 object created before `spec.storage` existed carries no quota, and 0 is NOT a
/// "controller default" — it would size the btrfs qgroup straight to zero. The quota of a legacy
/// source lives on its Volume, which is the object the controller sizes the disk from, so read it
/// there rather than inventing a number.
pub(super) const FALLBACK_QUOTA_GB: u64 = crd::DEFAULT_WS_QUOTA_GB;


pub(crate) async fn storage_quota(c: &kube::Client, storage: &Option<crd::WorkspaceStorage>, volume: &str) -> u64 {
    if let Some(st) = storage {
        return st.quota_gb;
    }
    let vols: Api<crd::Volume> = Api::all(c.clone());
    // Unreadable Volume: a copy sized at the standard quota beats one sized at zero, which cannot
    // be started at all.
    match vols.get_opt(volume).await {
        Ok(Some(v)) if v.spec.quota_gb > 0 => v.spec.quota_gb,
        _ => FALLBACK_QUOTA_GB,
    }
}


/// A lock answers ONE entry string, not a list: `nodejs@20 -> 20.20.2` stays true however the rest
/// of the list changed. So a restore carries every lock whose entry the new list still names, and
/// drops the rest — a request that swapped one entry does not invalidate the others' versions.
pub(super) fn locks_for(mut locks: Vec<crd::Lock>, packages: &[String]) -> Vec<crd::Lock> {
    locks.retain(|l| packages.contains(&l.entry));
    locks
}

// ── environments ─────────────────────────────────────────────────────────


#[cfg(test)]
mod tests {
    use super::ws_doc;
    use crate::crd;

    fn ws_fixture() -> crd::Workspace {
        crd::Workspace::new(
            "ws-1",
            crd::WorkspaceSpec {
                owner: "karthik".into(),
                team: String::new(),
                name: "web".into(),
                region: "centralindia".into(),
                image: crate::model::default_ws_image(),
                storage: None,
                desired_state: crd::DesiredState::Running,
                resources: Default::default(),
                packages: vec![],
                locks: vec![],
                attached_environment: None,
            },
        )
    }

    fn lock(entry: &str) -> crd::Lock {
        crd::Lock {
            entry: entry.into(),
            version: "20.20.2".into(),
            attr_path: "nodejs_20".into(),
            rev: "abc".into(),
            store_path: String::new(),
            resolved_at: "2026-09-08T00:00:00Z".into(),
            source: crd::LockSource::Nixhub,
        }
    }

    #[test]
    fn a_restore_keeps_the_locks_for_entries_the_new_list_still_names() {
        // Frozen: ["jq", "nodejs@20"] with the one lock that list needed.
        let frozen = vec![lock("nodejs@20")];
        let kept = super::locks_for(frozen.clone(), &["nodejs@20".to_string()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].version, "20.20.2");
        // The entry is gone from the list, so its lock answers nothing.
        assert!(super::locks_for(frozen, &["jq".to_string()]).is_empty());
    }

    #[test]
    fn a_workspace_doc_shows_the_spec_and_the_condition() {
        let mut w = ws_fixture();
        w.spec.packages = vec!["go".into()];
        w.status = Some(crd::WorkspaceStatus {
            conditions: vec![crd::condition(
                crd::PACKAGES_READY,
                false,
                "BuildFailed",
                "error: attribute 'jq2' missing",
                3,
            )],
            ..Default::default()
        });
        let d = ws_doc(&w, &Default::default());
        assert_eq!(d.packages, ["go"]);
        let ps = d.packages_status.unwrap();
        assert!(!ps.ready);
        assert_eq!(ps.reason, "BuildFailed");
        assert!(ps.message.contains("jq2"));
    }

    /// `Degraded/NodeDead` and `Decommissioning/NodeLeaving` are what the web turns into its two
    /// non-replication notices, so the doc must carry them or the page silently says nothing.
    #[test]
    fn a_workspace_doc_carries_degraded_and_decommissioning() {
        let mut w = ws_fixture();
        w.status = Some(crd::WorkspaceStatus {
            conditions: vec![
                crd::condition("Degraded", true, "NodeDead", "node n1 is down", 4),
                crd::condition("Decommissioning", true, "NodeLeaving", "this node is being retired", 4),
            ],
            ..Default::default()
        });
        let d = ws_doc(&w, &Default::default());
        let deg = d.degraded.expect("degraded must be shown");
        assert_eq!(deg.reason, "NodeDead");
        assert!(deg.message.contains("n1 is down"));
        let dec = d.decommissioning.expect("decommissioning must be shown");
        assert_eq!(dec.reason, "NodeLeaving");
        assert!(dec.message.contains("retired"));
    }

    /// A stopped workspace pinned to a dead node must warn what stopping costs; one with no
    /// `NodeDead` condition must not manufacture a warning out of an unrelated condition.
    #[test]
    fn stop_warns_only_when_the_pin_is_on_a_dead_node() {
        let dead = [crd::condition("Degraded", true, "NodeDead", "node n1 is down", 4)];
        let warning = super::node_dead_warning("n1", &dead).expect("must warn");
        assert!(warning.contains("n1"));
        assert!(warning.contains("will not follow the move"));

        let healthy = [crd::condition(crd::PACKAGES_READY, true, "Ready", "ok", 4)];
        assert!(super::node_dead_warning("n1", &healthy).is_none());
        assert!(super::node_dead_warning("n1", &[]).is_none());
    }
}
