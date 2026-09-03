//! `/v1/workspaces` — create, list, read, delete, start/stop, attach/detach, package edits,
//! clone and restore-to-new, plus the ssh connect ticket and the owner's platform key install.

use super::scope::{find_env, may_act_on, mine, my_ws, owned_by, owned_in, owners_namespaces, refuse_taken_name};
use super::{caller, check_region, guard_alloc, is_missing, kube, kube_err, not_found, not_ready, phase, rid, workspace_cost, ApiState};
use super::push::{clone_base, with_based_on};
use super::volumes::{find_snapshot, volume_region};
use crate::crd::{self, DesiredState, VolumeSource};
use crate::k8s::{labels, ATTACHED_ENV_LABEL, TEAM_LABEL};
use crate::model::*;
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

fn ws_doc(w: &crd::Workspace, pushed: &HashSet<String>) -> Workspace {
    let id = w.name_any();
    let st = w.status.as_ref();
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
fn bad_packages(e: crate::packages::PackageError) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": e.to_string()}))).into_response()
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

/// `0` is a qgroup nothing can start on, and the upper end is more than the pool node can back.
/// Clamped rather than refused: the web sends a fixed default, and a client that asks for more
/// than the ceiling gets the ceiling.
/// ponytail: one global ceiling; make it per-region node capacity if a region ever has more.
pub(crate) fn clamp_quota(gb: u64) -> u64 {
    gb.clamp(1, 500)
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
        // Lowercased BEFORE `may_act_on`: the directory's team slugs are lowercase, so a `may_act_on`
        // on the raw casing 404'd a real member of `acme` who typed `Acme`. 404, not 403, on a miss:
        // whether a team exists is not a non-member's to learn, same as every other owner-scoped route.
        Some(t) => {
            let t = t.to_lowercase();
            if may_act_on(&s, &owner, &t).await {
                t
            } else {
                return Err((StatusCode::NOT_FOUND, "no such team").into_response());
            }
        }
    };
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;
    let quota_gb = clamp_quota(body.quota_gb);
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
                .is_some_and(|(o, n)| rustic_git_storage::store::valid_owner(o)
                    && rustic_git_storage::store::valid_segment(n));
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
            attached_environment: None,
        },
    )
    .await?;
    // Off the request: the wait is up to 5 s of polling for a node to claim the object, and the
    // 202 already says "accepted, not done". `list_ws` re-installs an absent key regardless.
    tokio::spawn({
        let (s, c, owner, team, id) = (s.clone(), c.clone(), owner.clone(), team.clone(), id.clone());
        async move { install_user_key_after_placed(&s, &c, &owner, &team, &id).await }
    });
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &HashSet::new()))).into_response())
}

/// The one place a `Workspace` is written. Labels are a VIEW of `spec.owner`/`spec.team`, stamped
/// here so listings are indexed label selectors rather than scans.
async fn create_workspace(c: &kube::Client, id: &str, spec: crd::WorkspaceSpec) -> Result<crd::Workspace, Response> {
    let mut l = labels(&spec.owner, "workspace");
    l.insert(TEAM_LABEL.to_string(), spec.team.clone());
    let mut w = crd::Workspace::new(id, spec);
    w.metadata.labels = Some(l);
    let api: Api<crd::Workspace> = Api::all(c.clone());
    api.create(&PostParams::default(), &w).await.map_err(kube_err)
}

/// Put the owner's platform key in their workspace namespace, once a node has taken the workspace.
///
/// The namespace is the CONTROLLER's to make, so on a first workspace it does not exist at the
/// moment of the create. Waiting for the `Placed` condition — not for the namespace — is the
/// cheapest signal that a node has claimed the object and its OwnerBinding reconciler is running.
///
/// Best effort with a 5 s ceiling, because the key install is load-bearing but not worth failing a
/// create over: `list_ws` re-installs it when the Secret is absent, and that retry is what closes
/// the first-workspace-without-a-key gap for good.
///
/// The install itself stays best effort: the pod's key mount is optional (`k8s::user_key_volume`),
/// so a key that lands late — or never — costs the workspace its git identity, not its existence.
async fn install_user_key_after_placed(s: &ApiState, c: &kube::Client, owner: &str, team: &str, id: &str) {
    // Nothing to install and nothing to wait for.
    if s.keys.is_none() {
        return;
    }
    let api: Api<crd::Workspace> = Api::all(c.clone());
    for _ in 0..10 {
        if let Ok(Some(w)) = api.get_opt(id).await {
            if w.status.is_some_and(|st| {
                st.conditions.iter().any(|cd| cd.type_ == "Placed" && cd.status == "True")
            }) {
                write_user_key(s, c, &crd::ws_namespace(owner, team), owner).await;
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tracing::info!(%owner, workspace = %id, "not placed within 5s; the key install is left to the next list");
}

/// Rewrite the owner's key Secret in EVERY workspace namespace they have — what an ssh key add or
/// remove has to do for the change to reach a running workspace. The namespaces are found by the
/// owner label rather than by enumerating teams: the label is what the controller stamps on the
/// namespace it creates, so a team the api tier has never heard of is still covered.
pub async fn refresh_user_keys(s: &ApiState, owner: &str) {
    let Some(c) = s.kube.as_ref() else { return };
    let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(c.clone());
    let sel = format!("{}={owner},{}=workspace", crate::k8s::OWNER_LABEL, crate::k8s::KIND_LABEL);
    let list = match api.list(&ListParams::default().labels(&sel)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%owner, error = ?e, "could not list workspace namespaces to refresh keys");
            return;
        }
    };
    let mine = owners_namespaces(s, owner).await;
    for ns in list.items.iter().map(|n| n.name_any()) {
        if !mine.contains(&ns) {
            tracing::warn!(%owner, namespace = %ns, "namespace carries the owner label but is not theirs by name");
            continue;
        }
        write_user_key(s, c, &ns, owner).await;
    }
}

async fn write_user_key(s: &ApiState, c: &kube::Client, ns: &str, owner: &str) {
    let Some(store) = &s.keys else { return };
    let private = match store.user_key(owner).await {
        Ok(Some(p)) => p,
        Ok(None) => return, // never generated one; /v1/platform-key makes it on first read
        Err(e) => {
            tracing::warn!(%owner, error = ?e, "could not read the platform key");
            return;
        }
    };
    let api: Api<k8s_openapi::api::core::v1::Secret> = Api::namespaced(c.clone(), ns);
    // A failed lookup writes NOTHING rather than an empty file: an empty `authorized_keys` locks
    // the owner out of a workspace they can otherwise reach, and the next call rewrites it anyway.
    // Unwired (dev, no directory) writes NOTHING for the same reason a failed lookup does: an
    // empty `authorized_keys` is not "no keys yet", it is the owner locked out of their workspace.
    let Some(lookup) = &s.directory else { return };
    let Some(material) = lookup.for_owner(owner).await else {
        tracing::warn!(%owner, "could not read the owner's ssh keys; leaving the secret alone");
        return;
    };
    let secret = crate::k8s::user_key_secret(owner, ns, &private, &material);
    if let Err(e) = api
        .patch(
            crate::k8s::USER_KEY_SECRET,
            &kube::api::PatchParams::apply("rustic-git-api").force(),
            &kube::api::Patch::Apply(&secret),
        )
        .await
    {
        tracing::warn!(%owner, error = ?e, "could not install the platform key in the namespace");
    }
}

pub(crate) async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    // `?team=` scopes the list to the caller's workspaces IN that team; absent means personal.
    // Membership is checked so the answer for a team the caller is not in is 404, not an empty
    // list that says the team exists.
    let team = match q.get("team").map(|t| t.trim()).filter(|t| !t.is_empty() && *t != owner.name) {
        None => String::new(),
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
const GATEWAY_DOMAIN: &str = "khost.dev";

fn gateway_url(region: &str, id: &str) -> String {
    format!("wss://ws-{region}.{GATEWAY_DOMAIN}/tunnel/{id}")
}

/// A connect ticket for `kl ssh`: a short-lived token naming this workspace, where to take it, and
/// the host key to pin. Nothing is stored — the token is signed, and the gateway verifies it.
///
/// `{id}` may also be a NAME: `kl ws ssh <name>` used to list every workspace just to translate
/// one, and did it twice more in the ProxyCommand. An exact id wins so a workspace named after
/// another's id cannot shadow it; only the caller's own workspaces are searched, and the answer
/// carries the id it resolved to.
pub(crate) async fn ssh_session(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(target): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = match my_ws(&s, &owner, &target).await {
        Ok(w) => w,
        Err(_) => {
            let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
            api.list(&owned_by(&owner))
                .await
                .map_err(kube_err)?
                .items
                .into_iter()
                .filter(|w| w.spec.owner == owner.name)
                .find(|w| w.spec.name == target)
                .ok_or_else(not_found)?
        }
    };
    let id = w.metadata.name.clone().ok_or_else(not_found)?;
    let st = w.status.as_ref();
    let phase = st.map(|st| st.phase.as_str()).unwrap_or("creating");
    if phase != "ready" {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("workspace is {phase}")})),
        )
            .into_response());
    }
    // No host key means no way to pin the connection, and a TOFU prompt for a key the platform is
    // about to know is exactly what this design refuses.
    let Some(host_key) = st.and_then(|st| st.ssh_host_key.clone()) else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "the workspace has not reported its host key yet")
            .into_response());
    };
    let (token, claims) = s.jwt.mint_ssh_session(&owner, &id, &w.spec.region).map_err(|e| {
        tracing::error!(error = %e, "mint ssh session");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not mint a session").into_response()
    })?;
    let expires_at = chrono::DateTime::from_timestamp(claims.exp as i64, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            "token": token,
            "gateway": gateway_url(&w.spec.region, &id),
            "expires_at": expires_at,
            "host_key": host_key,
        })),
    )
        .into_response())
}

/// ONE delete. The "Workspace first, then Volume" ordering became the API server's job the moment
/// the Volume got an ownerReference: garbage collection follows it, and the Volume's own finalizer
/// still holds the reclaim until the subvolume is gone.
pub(crate) async fn delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let c = kube(&s)?;
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
    if let Err(e) = ws.delete(&id, &DeleteParams::default()).await {
        if !is_missing(&e) {
            return Err(kube_err(e));
        }
    }
    drop_attach_policy(c, &id, env.as_deref()).await;
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
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    set_desired::<crd::Workspace>(kube(&s)?, &id, DesiredState::Stopped).await?;
    // Every non-204 success is `res.json()`'d by the web client (web/apps/web/src/lib/api.ts) —
    // a body-less 202 throws there, so this always emits an object, `warning` present only when
    // there is one to give.
    // The whole doc, not a bare `{}`: the caller needs `replicated` to know whether this may be
    // started elsewhere, and a second round trip for it would race the stop it just asked for.
    let warning = w.status.as_ref().and_then(|st| node_dead_warning(&st.node_name, &st.conditions));
    // The real pushed set, not `HashSet::new()`: an empty one made `volume` null on every mutation
    // response even for a volume with fifty pushes, and a client reading that as "never pushed" got
    // a wrong answer from all seven of these handlers.
    let pushed = pushed_volumes(&s, kube(&s)?, &owner).await?;
    let mut doc = ws_doc(&w, &pushed);
    doc.state = WsState::Stopped;
    let mut body = serde_json::to_value(&doc).expect("Workspace doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct AttachBody {
    environment: String,
}

/// Attach this workspace to an environment, so its services resolve by bare name.
///
/// A merge patch on the one field, for the same reason `set_desired` is one: this handler was sent
/// one field and must not claim ownership of a spec the caller never wrote. Spec only — every
/// visible effect of an attachment is the agent's reconcile.
pub(crate) async fn attach_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AttachBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    // Same predicate `validate_ws_spec` applies to this field at the agent — checked here too so a
    // bad id is a 422 at the door rather than a kube 422 (a patch on an illegal label value)
    // laundered into a 500 further down.
    if !valid_segment_label(&body.environment) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "invalid environment id").into_response());
    }
    // `find_env` answers 404 for an environment the caller has no part in, which is what keeps this
    // route from being a way to enumerate other people's environments.
    let e = find_env(&s, &owner, &body.environment).await?;
    if e.spec.region != w.spec.region {
        // Another region is another cluster: no pod route, no DNS. Refused here rather than left to
        // fail inside a reconcile that has no way to report it back to this caller.
        return Err((StatusCode::CONFLICT, "the environment is in another region, which is another cluster").into_response());
    }
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    // The label is stamped here, not left for the next reconcile: `delete_env`'s sweep selects on
    // it, and a window where the spec says attached but the label does not would let a delete
    // racing this call miss the workspace it needs to clear.
    let patch = serde_json::json!({
        "spec": {"attachedEnvironment": body.environment},
        "metadata": {"labels": {ATTACHED_ENV_LABEL: body.environment}},
    });
    api.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Delete the environment-side half of an attachment grant, which lives in a namespace the
/// Workspace's ownerReference cannot reach. Best-effort with a warning: the environment's own
/// deletion collects it either way, and a grant left behind is dormant until something re-adds an
/// egress with the same workspace id.
async fn drop_attach_policy(c: &kube::Client, id: &str, env: Option<&str>) {
    let Some(env) = env else { return };
    let policies: Api<k8s_openapi::api::networking::v1::NetworkPolicy> =
        Api::namespaced(c.clone(), &crd::env_namespace(env));
    if let Err(e) = policies.delete(&crate::k8s::attach_policy_name(id), &DeleteParams::default()).await {
        tracing::warn!(workspace = %id, environment = %env, error = %e, "removing the environment-side attach policy");
    }
}

/// Detach. Idempotent: a workspace that is not attached is already in the state being asked for.
pub(crate) async fn detach_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let env = crd::attached_environment(&w);
    let c = kube(&s)?.clone();
    let api: Api<crd::Workspace> = Api::all(c.clone());
    // `null` is how a merge patch REMOVES a key. `""` would leave the reconciler resolving an
    // environment named empty-string. The label is cleared in the same patch, for the same reason
    // it is stamped in the same patch on attach.
    let patch = serde_json::json!({
        "spec": {"attachedEnvironment": serde_json::Value::Null},
        "metadata": {"labels": {ATTACHED_ENV_LABEL: serde_json::Value::Null}},
    });
    api.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    // A STOPPED workspace never reaches the attach block of a reconcile — `apply_workspace` returns
    // at the stop gate — so the agent would never collect the environment-side half, and clearing
    // the spec destroys the `Attached` condition that addresses it. Collect it here, after the
    // patch so a concurrent pass cannot re-`ensure` what was just removed. For a RUNNING workspace
    // this merely races the reconcile to the same delete, which is idempotent.
    drop_attach_policy(&c, &id, env.as_deref()).await;
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct PackagesBody {
    packages: Vec<String>,
}

/// Change the declared package list. A merge patch on `spec.packages` alone, for the same reason
/// `set_desired` is one: this handler was sent one field and must not claim ownership of a spec
/// the caller never wrote.
pub(crate) async fn patch_ws_packages(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PackagesBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    my_ws(&s, &owner, &id).await?;
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    let patch = serde_json::json!({"spec": {"packages": body.packages}});
    let w = api
        .patch(&id, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    let pushed = pushed_volumes(&s, kube(&s)?, &owner).await?;
    Ok(Json(ws_doc(&w, &pushed)).into_response())
}

#[derive(serde::Deserialize)]
pub(crate) struct CloneBody {
    pub(crate) name: String,
}

/// The one local-copy route.
///
/// It names no node: placement is the claim's job now, and the ONE rule — a node up to date for the
/// SOURCE worktree — is read there. At the instant of the cut above the owner is simply the only
/// node that qualifies, so a running source's clone lands on the owner by arithmetic; there is no
/// "same node" rule here or anywhere.
pub(crate) async fn clone_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    check_ws_name(&body.name)?;
    let src = my_ws(&s, &owner, &id).await?;
    refuse_taken_name(kube(&s)?, &owner, &src.spec.team, &body.name).await?;
    let c = kube(&s)?;
    let new_id = rid("ws");
    let volume = ws_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
    let owner_of = if src.spec.team.is_empty() { owner.name.clone() } else { src.spec.team.clone() };
    guard_alloc(&s, &owner_of, !src.spec.team.is_empty(), &workspace_cost(quota, &src.spec.resources)).await?;
    // A clone is a second worktree of the SOURCE's own volume, pinned to a cut taken NOW — resolved
    // ONCE, here, so the clone never drifts with the source's later pushes and never lags whatever
    // the last sync beat happened to leave.
    let interrupted = src.status.as_ref().is_some_and(|st| interrupted(&st.conditions));
    let (based_on, cut) =
        clone_base(c, &owner, &volume, &id, interrupted, src.controller_owner_ref(&()), crd::SnapshotState::of_workspace(&src)).await?;
    // An interrupted source is the ONE case that cannot be a second worktree of the source's own
    // volume: that volume is pinned to the node that is down, so the peer holding the cut would
    // settle `Degraded=NodeMismatch` instead of starting. It gets its own volume, seeded from the
    // held cut — see `VolumeSource::SeededFrom`. Every other clone is unchanged.
    let source = if based_on.interrupted {
        VolumeSource::SeededFrom { volume, snapshot: based_on.snapshot.clone() }
    } else {
        VolumeSource::CloneOf { volume, commit: Some(based_on.snapshot.clone()) }
    };
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner: owner.name.clone(),
            // A clone lives where its source lives: same team, same namespace.
            team: src.spec.team.clone(),
            name: body.name,
            region: src.spec.region.clone(),
            image: src.spec.image.clone(),
            storage: Some(crd::WorkspaceStorage { quota_gb: quota, source: Some(source) }),
            desired_state: DesiredState::Running,
            resources: Default::default(),
            packages: src.spec.packages.clone(),
            attached_environment: None,
        },
    )
    .await?;
    // The cut LAST: the workspace already exists and names it, so nothing can leave a `Working`
    // Snapshot behind that no clone will ever consume and every later clone would 409 on.
    if let Some(snap) = cut {
        let api: Api<crd::Snapshot> = Api::all(c.clone());
        api.create(&PostParams::default(), &snap).await.map_err(kube_err)?;
    }
    let pushed = pushed_volumes(&s, c, &owner).await?;
    Ok(with_based_on(&ws_doc(&w, &pushed), &based_on))
}

/// What a copy of `volume` should be sized at.
///
/// A release-1 object created before `spec.storage` existed carries no quota, and 0 is NOT a
/// "controller default" — it would size the btrfs qgroup straight to zero. The quota of a legacy
/// source lives on its Volume, which is the object the controller sizes the disk from, so read it
/// there rather than inventing a number.
const FALLBACK_QUOTA_GB: u64 = crd::DEFAULT_WS_QUOTA_GB;

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

#[derive(serde::Deserialize)]
pub(crate) struct RestoreBody {
    name: String,
    // The `snapshot_id` alone is a Snapshot CR name — the old registry-scoped `volume`
    // hint that used to turn a multi-volume scan into one read no longer means anything, since
    // `find_snapshot` looks the CR up by name directly.
    snapshot_id: String,
    // All optional and all overrides: absent means "whatever the snapshot froze", not "the
    // default" — restoring last month's files with today's image is not last month's workspace.
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    packages: Option<Vec<String>>,
    // No `resources` rung on purpose: nothing user-facing offers to size a restore (create and
    // clone both hardcode the default), and an unclamped body field here would let a caller
    // reserve a node's whole capacity. Resources come from the frozen state, then the live
    // source, then the default.
    #[serde(default)]
    quota_gb: Option<u64>,
    #[serde(default)]
    attached_environment: Option<String>,
}

/// New workspace grafted onto an explicit, possibly-older snapshot — a PUSHED snapshot, which is
/// what makes this different from `clone` (always a copy of the current state).
///
/// The snapshot is resolved against the SERVER tier's history, not a live workspace: restoring is
/// most useful precisely when the original is gone, and requiring `my_ws(src)` first is what made
/// a deleted workspace's snapshots unrestorable.
pub(crate) async fn restore_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    check_ws_name(&body.name)?;
    // Restore-to-new IS a clone at a named snapshot: under the snapshot model there is no
    // registry to fetch from any more, so this resolves the request's `snapshot_id` — a `Snapshot`
    // CR name — straight against the CRD, and the new workspace's source becomes
    // `CloneOf{volume, commit: Some(id)}`, exactly `Engine::clone_local_ids`/`checkout`'s own
    // shared-worktree path. `find_snapshot` is the owner check:
    // CR exists, Ready, and the caller may read `spec.owner` — anything else is a 404, same as a
    // missing snapshot, so a caller learns nothing about volumes that are not theirs.
    let snap = find_snapshot(&s, &owner, None, &body.snapshot_id).await?;
    let volume = snap.spec.volume.clone();

    // A `state` from the other kind is a request to refuse, not to half-honour: restoring an
    // environment snapshot as a workspace mounts a database's data directory under the default
    // image with no packages. `None` is a snapshot cut before states existed — "absent means old",
    // and every reader keeps its fallback for it. Checked before any other lookup so the refusal
    // costs nothing beyond the snapshot fetch already made.
    let frozen = match &snap.spec.state {
        Some(crd::SnapshotState::Workspace { image, packages, resources, quota_gb, attached_environment }) => {
            Some((image.clone(), packages.clone(), resources.clone(), *quota_gb, attached_environment.clone()))
        }
        Some(crd::SnapshotState::Environment { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "this snapshot was cut from an environment; use POST /v1/environments/restore",
            )
                .into_response())
        }
        None => None,
    };

    // A live source still knows its own size and settings; a deleted one gets the standard quota.
    // `my_ws(volume)` resolves only because an OWNED volume shares its parent workspace's id — the
    // one case this can look up. A shared-clone volume's id is the SOURCE workspace's, so this
    // resolves the source, and the team/region a clone contributes below are the source's on
    // purpose: a snapshot taken on a shared worktree has no other owner to ask.
    let src = my_ws(&s, &owner, &volume).await.ok();
    let team = src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default();
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;

    // Precedence: the request, then what the snapshot froze, then the live source, then defaults.
    // A snapshot's `state` is DATA — written by an agent, hand-editable in the cluster — so every
    // value it contributes goes through the same checks a request body's does, below.
    let image = body
        .image
        .clone()
        .or_else(|| frozen.as_ref().map(|f| f.0.clone()))
        .or_else(|| src.as_ref().map(|w| w.spec.image.clone()))
        .unwrap_or_else(default_ws_image);
    let packages = body
        .packages
        .clone()
        .or_else(|| frozen.as_ref().map(|f| f.1.clone()))
        .or_else(|| src.as_ref().map(|w| w.spec.packages.clone()))
        .unwrap_or_default();
    crate::packages::validate_list(&packages).map_err(bad_packages)?;
    let resources = frozen
        .as_ref()
        .map(|f| f.2.clone())
        .or_else(|| src.as_ref().map(|w| w.spec.resources.clone()))
        .unwrap_or_default();
    let quota = match (body.quota_gb, &frozen, &src) {
        (Some(q), _, _) => clamp_quota(q),
        (None, Some(f), _) => clamp_quota(f.3),
        (None, None, Some(w)) => storage_quota(c, &w.spec.storage, &volume).await,
        // A deleted source cannot be asked its size, and nothing user-facing offers to name one:
        // someone recovering a lost workspace is not sizing a disk. The standard quota, which is
        // also what `create` sends by default.
        (None, None, None) => FALLBACK_QUOTA_GB,
    };
    // An attachment the caller cannot see is dropped rather than refused: the environment may
    // simply be gone or someone else's now, and that must not make the snapshot unrestorable.
    // `find_env` is the same visibility check `attach_ws` applies.
    let attached_environment = match body.attached_environment.clone().or_else(|| frozen.as_ref().and_then(|f| f.4.clone())) {
        // Only a 404 is "gone, or not mine". An unreachable API server is a 5xx and must be
        // reported as one, not laundered into a silently unattached workspace.
        Some(e) => match find_env(&s, &owner, &e).await {
            Ok(_) => Some(e),
            Err(r) if r.status() == StatusCode::NOT_FOUND => None,
            Err(r) => return Err(r),
        },
        None => None,
    };
    // A restore is an allocation like any other: the snapshot survives the refusal untouched, so
    // the person can raise their quota and try the same id again.
    let owner_of = if team.is_empty() { owner.name.clone() } else { team.clone() };
    guard_alloc(&s, &owner_of, !team.is_empty(), &workspace_cost(quota, &resources)).await?;
    let new_id = rid("ws");
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner: owner.name.clone(),
            team: src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default(),
            name: body.name,
            // No per-snapshot region under the snapshot model (single-pool, replica-based; cross-
            // region restore is out of scope — see the design doc). A live source still knows its
            // own; for a deleted one the detached Volume holding the bytes does.
            region: match src.as_ref() {
                Some(w) => w.spec.region.clone(),
                None => volume_region(c, &volume).await.unwrap_or_else(|| "default".to_string()),
            },
            image,
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                source: Some(VolumeSource::CloneOf { volume, commit: Some(body.snapshot_id) }),
            }),
            desired_state: DesiredState::Running,
            resources,
            packages,
            attached_environment,
        },
    )
    .await?;
    let pushed = pushed_volumes(&s, c, &owner).await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &pushed))).into_response())
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
                attached_environment: None,
            },
        )
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
