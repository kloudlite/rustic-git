//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation writes a CUSTOM RESOURCE and answers 202 with a projection of it. The object is
//! the work item: there is no queue, no lease and no dispatch — the node named by `spec.nodeName`
//! reconciles what it owns. `Region` alone still lives in Cosmos (`store`), because it is
//! cross-cluster metadata no single API server can hold.
//!
//! Auth mirrors `crates/api`'s `caller()`: a Bearer JWT identifies the owner. There is no
//! existing "is this caller an admin" check anywhere in the codebase to reuse (grepped for one —
//! none exists), so region routes gate on a small static allowlist of emails passed in at
//! construction (`RUSTIC_GIT_WORKSPACES_ADMINS` in the api bin). Upgrade path: a real roles
//! table, if more than one admin-gated surface ever shows up.

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::crd::{self, DesiredState, VolumeSource};
use crate::registry::CommitRecord;
use futures::StreamExt;
use crate::model::*;
use crate::store::MetaStore;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::ResourceExt;
use std::collections::BTreeMap;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rustic_git_core::httpx::bearer_token;
use rustic_git_core::jwt::Jwt;
use std::collections::HashSet;
use std::sync::Arc;

/// Team-membership lookup, kept behind a trait rather than a direct dependency on
/// `rustic_git_pulls::directory::Directory` (mongo-backed, heavy to construct) so unit tests can
/// supply a closure/stub instead. Production wires `Directory` in via an adapter in `bins/api`.
/// One method only: "is this caller in this team" reduces to "which teams is the caller in", and
/// list_env needs the full list anyway.
#[async_trait::async_trait]
pub trait MembershipCheck: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;
}

pub struct ApiState {
    pub store: Arc<dyn MetaStore>,
    pub jwt: Arc<Jwt>,
    /// Emails allowed to hit the admin-gated region routes. See module docs.
    pub admins: HashSet<String>,
    /// Team lookups for team-owned environments (see module docs' Part 2). `None` means no
    /// directory is wired (dev, or the directory tier is down) — team envs answer 503 rather than
    /// silently behaving as if the caller has no teams.
    pub membership: Option<Arc<dyn MembershipCheck>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace, environment and
    /// volume route answers 503 rather than not existing.
    pub kube: Option<kube::Client>,
    /// The auth store, solely so workspace creation can copy the owner's platform-issued git key
    /// into their namespace. `None` in dev and in tests: workspaces still create, they just come
    /// up without a key.
    pub keys: Option<Arc<rustic_git_storage::store::Store>>,
    /// The server tier's browse routes, where a volume's snapshots actually live. `None` in dev
    /// and in tests that do not exercise them: the volume routes answer 503, the same way every
    /// other route here reports a missing dependency rather than pretending it does not exist.
    pub upstream: Option<Arc<crate::upstream::Upstream>>,
}

impl ApiState {
    pub fn new(store: Arc<dyn MetaStore>, jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState { store, jwt, admins, membership: None, kube: None, keys: None, upstream: None }
    }

    pub fn with_membership(mut self, membership: Arc<dyn MembershipCheck>) -> Self {
        self.membership = Some(membership);
        self
    }

    pub fn with_kube(mut self, client: kube::Client) -> Self {
        self.kube = Some(client);
        self
    }

    pub fn with_keys(mut self, keys: Arc<rustic_git_storage::store::Store>) -> Self {
        self.keys = Some(keys);
        self
    }

    pub fn with_upstream(mut self, upstream: Arc<crate::upstream::Upstream>) -> Self {
        self.upstream = Some(upstream);
        self
    }
}

async fn teams_for(s: &ApiState, caller: &str) -> Vec<String> {
    match &s.membership {
        Some(m) => m.teams_for(caller).await,
        None => Vec::new(),
    }
}

/// `owner` is the environment's actual owner field (a username or a team slug). Personal envs
/// (`owner == caller`) always pass; a team env passes when the caller is a member.
async fn may_act_on(s: &ApiState, caller: &str, owner: &str) -> bool {
    caller == owner || teams_for(s, caller).await.iter().any(|t| t == owner)
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/regions", post(create_region).get(list_regions))
        .route("/v1/regions/{id}/rotate-token", post(rotate_region_token))
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/restore", post(restore_ws))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws).patch(patch_ws_packages))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/environments", post(create_env).get(list_env))
        // Before `/{id}`: `restore` is a verb, not an environment id.
        .route("/v1/environments/restore", post(restore_env))
        .route("/v1/environments/{id}", get(get_env).delete(delete_env))
        .route("/v1/environments/{id}/start", post(start_env))
        .route("/v1/environments/{id}/stop", post(stop_env))
        .route("/v1/environments/{id}/clone", post(clone_env))
        .route("/v1/environments/{id}/push", post(push_env))
        .route("/v1/environments/{id}/restore-in-place", post(restore_env_in_place))
        .route("/v1/volumes", get(list_volumes))
        .route("/v1/volumes/{name}/history", get(volume_history))
        .route("/v1/volumes/{name}", axum::routing::delete(delete_volume))
        .route(
            "/v1/volumes/{name}/snapshots/{snapshot}",
            axum::routing::delete(delete_snapshot),
        )
        .route("/v1/volumes/{name}/refs", get(volume_refs))
        .with_state(state)
}

fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", rustic_git_core::hex(&b))
}

/// Header carrying the per-region agent token, mirroring `rustic_git_core::peer::PEER_HEADER`'s
/// naming and constant-time-compare style.
pub const WS_AGENT_HEADER: &str = "x-rustic-git-ws-agent-token";

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    rustic_git_core::hex(&b)
}

/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    let c = state.jwt.verify(tok.trim()).map_err(|_| unauthorized())?;
    c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
}

fn require_admin(state: &ApiState, email: &str) -> Result<(), Response> {
    if state.admins.contains(email) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "admin only").into_response())
    }
}

fn store_err(e: crate::store::StoreErr) -> Response {
    use crate::store::StoreErr::*;
    match e {
        NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        Conflict | CasFailed => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        Other(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
    }
}

// ── regions ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    storage_account: String,
    blob_container: String,
    /// `active` or `inactive`. Re-registering a region is the only way to retire one — there is
    /// no delete — and a retired region must stop being offered to new workspaces while its
    /// existing records stay readable.
    #[serde(default = "active_status")]
    status: String,
}

fn active_status() -> String {
    "active".into()
}

/// Mint a fresh agent token for an existing region, returning it once.
///
/// `create_region` deliberately PRESERVES an existing token, so re-registering a region cannot
/// rotate it — which left a leaked agent token with no way to be revoked short of editing the
/// store by hand. That is the gap this closes: a token that cannot be rotated is a token that
/// stays valid forever after it leaks.
///
/// The new token is returned in the response and nowhere else, the same contract `create_region`
/// has for a first mint. Every agent in the region must be updated before or shortly after this
/// call — the old token stops working the moment it lands.
async fn rotate_region_token(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let tok = bearer_token(&headers).ok_or_else(unauthorized)?;
    let email = s.jwt.verify(tok.trim()).map(|c| c.sub).map_err(|_| unauthorized())?;
    require_admin(&s, &email)?;

    let mut r = s
        .store
        .regions()
        .await
        .map_err(store_err)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(not_found)?;
    r.agent_token = random_token();
    s.store.put_region(&r).await.map_err(store_err)?;
    Ok((StatusCode::OK, Json(r)).into_response())
}

async fn create_region(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRegion>,
) -> Result<Response, Response> {
    // Admin gating keys on the EMAIL (the allowlist's identity), not the username `caller`
    // resolves — an admin needs no username to register regions.
    let tok = bearer_token(&headers).ok_or_else(unauthorized)?;
    let email = s.jwt.verify(tok.trim()).map(|c| c.sub).map_err(|_| unauthorized())?;
    require_admin(&s, &email)?;
    // Existing region re-registered without a token yet: generate and persist one now rather
    // than leaving agents unable to authenticate. Returned once, here, on the create response —
    // callers must save it (same shape as any bearer secret minted on creation).
    let agent_token =
        s.store.regions().await.map_err(store_err)?.into_iter().find(|r| r.id == body.id).and_then(|r| {
            if r.agent_token.is_empty() {
                None
            } else {
                Some(r.agent_token)
            }
        });
    let r = Region {
        id: body.id,
        name: body.name,
        storage_account: body.storage_account,
        blob_container: body.blob_container,
        status: if body.status == "inactive" { "inactive".into() } else { "active".into() },
        agent_token: agent_token.unwrap_or_else(random_token),
    };
    s.store.put_region(&r).await.map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(r)).into_response())
}

async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers)?;
    // The token is a secret, returned once on creation only — never echoed back on list.
    let mut regions = s.store.regions().await.map_err(store_err)?;
    for r in &mut regions {
        r.agent_token.clear();
    }
    Ok(Json(regions).into_response())
}

// ── the cluster ──────────────────────────────────────────────────────────

fn kube(s: &ApiState) -> Result<&kube::Client, Response> {
    s.kube.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "kubernetes not configured on this node").into_response()
    })
}

/// An API-server error keeps its own status where the caller can act on it (404 is "no such
/// workspace", 409 is "retry"); anything else is ours, not the caller's.
fn kube_err(e: kube::Error) -> Response {
    match &e {
        kube::Error::Api(ae) if ae.code == 404 => not_found(),
        kube::Error::Api(ae) if ae.code == 409 => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub const OWNER_LABEL: &str = "rustic-git.io/owner";
pub const KIND_LABEL: &str = "rustic-git.io/kind";
/// The team a workspace was made in; empty for personal. A listing filter, like the other two —
/// `spec.team` is the truth and the controller re-stamps this from it.
pub const TEAM_LABEL: &str = "rustic-git.io/team";

/// A label selector is the list filter, not a field selector: `metadata.labels` is indexed for
/// selectors by every API server, while an arbitrary spec field needs a `selectableFields` entry —
/// and adding one per query axis is how a CRD becomes a database.
fn owned_by(owner: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"))
}

/// One person's workspaces in one team (empty = personal). Both labels, so a team page never
/// shows the personal ones and the personal page never shows a team's.
fn owned_in(owner: &str, team: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner},{TEAM_LABEL}={team}"))
}

fn labels(owner: &str, kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNER_LABEL.to_string(), owner.to_string()),
        (KIND_LABEL.to_string(), kind.to_string()),
    ])
}

/// `status.phase` is the state, and an object the controller has not seen yet has no status at
/// all — `creating` rather than a `null` the web app's enum cannot parse.
fn phase<T: serde::de::DeserializeOwned>(p: Option<&str>, default: T) -> T {
    p.and_then(|p| serde_json::from_value(serde_json::json!(p)).ok()).unwrap_or(default)
}

/// The child `Volume`'s name. STATUS first: the reconciler creates the Volume and then reports it,
/// so that is the fact. `spec.volumeRef` is the deprecated release-1 fallback for an object created
/// before placement moved into status — Task 11 drops it, and this helper is the one place to edit.
fn ws_volume(w: &crd::Workspace) -> Option<&str> {
    w.status
        .as_ref()
        .and_then(|st| st.volume_ref.as_deref())
        .or(w.spec.volume_ref.as_deref())
        .filter(|v| !v.is_empty())
}

/// `env_doc`'s half of the same rule; see `ws_volume`.
fn env_volume(e: &crd::Environment) -> Option<&str> {
    e.status
        .as_ref()
        .and_then(|st| st.volume_ref.as_deref())
        .or(e.spec.volume_ref.as_deref())
        .filter(|v| !v.is_empty())
}

/// Every volume of `owner` that has ever landed a snapshot.
///
/// This replaces `Volume.status.lastPush`, and it is a QUERY rather than a field because a field
/// would need a second controller writing the Volume's status — `patch_status` force-applies under
/// one field manager, so the Volume reconciler's next pass would prune it (server-side apply
/// removes fields a manager previously owned and no longer sets).
///
/// ONE label list per REQUEST, passed down to every row: one lookup per row turns a listing into an
/// N+1 against the API server.
async fn pushed_volumes(c: &kube::Client, owner: &str) -> Result<HashSet<String>, Response> {
    let api: Api<crd::SnapshotRequest> = Api::all(c.clone());
    Ok(api
        .list(&owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Done))
        .map(|r| r.spec.volume)
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
        // Free-form live state was a job-era field the agent wrote back into the doc; the pod and
        // its status are the live state now. Kept in the body so the web app's parse is unchanged.
        live_state: serde_json::Value::Null,
        packages: w.spec.packages.clone(),
        base_packages: st.and_then(|s| s.packages.as_ref()).map(|p| p.base.clone()).unwrap_or_default(),
        packages_status: st.and_then(|s| {
            s.conditions.iter().find(|c| c.type_ == crd::PACKAGES_READY).map(|c| PackagesDoc {
                ready: c.status == "True",
                reason: c.reason.clone(),
                message: c.message.clone(),
            })
        }),
        id,
    }
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
        id,
    }
}

/// Flip `spec.desiredState`. A merge patch, not an apply: this touches one field and must not
/// claim ownership of the rest of a spec the caller never sent.
async fn set_desired<K>(c: &kube::Client, id: &str, want: DesiredState) -> Result<(), Response>
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
struct NewWorkspace {
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

async fn create_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewWorkspace>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    let team = match body.team.as_deref().map(str::trim).filter(|t| !t.is_empty() && *t != owner) {
        None => String::new(),
        // 404, not 403: whether a team exists is not a non-member's to learn, same as every
        // other owner-scoped route.
        Some(t) if may_act_on(&s, &owner, t).await => t.to_lowercase(),
        Some(_) => return Err((StatusCode::NOT_FOUND, "no such team").into_response()),
    };
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
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
            owner: owner.clone(),
            team: team.clone(),
            name: body.name,
            region: body.region,
            image: body.image,
            storage: Some(crd::WorkspaceStorage { quota_gb: body.quota_gb, source }),
            desired_state: DesiredState::Running,
            restore: None,
            resources: Default::default(),
            node_name: None,
            volume_ref: None,
            packages: body.packages,
        },
    )
    .await?;
    install_user_key_after_placed(&s, c, &owner, &team, &id).await;
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
                install_user_key(s, c, owner, team).await;
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tracing::info!(%owner, workspace = %id, "not placed within 5s; the key install is left to the next list");
}

/// Put the owner's platform key in their workspace namespace, if there is one to put.
///
/// Best effort on purpose, and not a step the request waits on succeeding: the namespace is the
/// CONTROLLER's to create, so on a first workspace it very likely does not exist yet. The pod's
/// mount is optional (`k8s::user_key_volume`), so a key that lands on the next create — or never —
/// costs the workspace its git identity, not its existence.
/// ponytail: no retry, so a user's first workspace has no key until they make a second one; move
/// this to the controller if that shows up as a complaint.
async fn install_user_key(s: &ApiState, c: &kube::Client, owner: &str, team: &str) {
    let Some(store) = &s.keys else { return };
    let private = match store.user_key(owner).await {
        Ok(Some(p)) => p,
        Ok(None) => return, // never generated one; /v1/platform-key makes it on first read
        Err(e) => {
            tracing::warn!(%owner, error = ?e, "could not read the platform key");
            return;
        }
    };
    let api: Api<k8s_openapi::api::core::v1::Secret> =
        Api::namespaced(c.clone(), &crd::ws_namespace(owner, team));
    let secret = crate::k8s::user_key_secret(owner, team, &private);
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

async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    // `?team=` scopes the list to the caller's workspaces IN that team; absent means personal.
    // Membership is checked so the answer for a team the caller is not in is 404, not an empty
    // list that says the team exists.
    let team = match q.get("team").map(|t| t.trim()).filter(|t| !t.is_empty() && *t != owner) {
        None => String::new(),
        Some(t) if may_act_on(&s, &owner, t).await => t.to_lowercase(),
        Some(_) => return Err((StatusCode::NOT_FOUND, "no such team").into_response()),
    };
    // No "filter out the deleted ones": a deleted object is gone from the API server.
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let items = api.list(&owned_in(&owner, &team)).await.map_err(kube_err)?.items;
    let pushed = pushed_volumes(c, &owner).await?;
    let list: Vec<_> = items.iter().map(|w| ws_doc(w, &pushed)).collect();
    // The retry the create's 5 s ceiling defers to: cheap, idempotent, and the only place a user
    // whose very first workspace outran its namespace is ever seen again. Seeded pods REQUIRE the
    // key mount, so "it lands next time" is not good enough on its own.
    if !items.is_empty() && s.keys.is_some() {
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(c.clone(), &crd::ws_namespace(&owner, &team));
        if matches!(secrets.get_opt(crate::k8s::USER_KEY_SECRET).await, Ok(None)) {
            install_user_key(&s, c, &owner, &team).await;
        }
    }
    Ok(Json(list).into_response())
}

/// Workspaces are strictly personal — no team ownership — so ownership is a field comparison, and
/// someone else's workspace is a 404, never a 403.
async fn my_ws(s: &ApiState, owner: &str, id: &str) -> Result<crd::Workspace, Response> {
    let api: Api<crd::Workspace> = Api::all(kube(s)?.clone());
    let w = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if w.spec.owner != owner {
        return Err(not_found());
    }
    Ok(w)
}

async fn get_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let w = my_ws(&s, &owner, &id).await?;
    let pushed = pushed_volumes(kube(&s)?, &owner).await?;
    Ok(Json(ws_doc(&w, &pushed)).into_response())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// ONE delete. The "Workspace first, then Volume" ordering became the API server's job the moment
/// the Volume got an ownerReference: garbage collection follows it, and the Volume's own finalizer
/// still holds the reclaim until the subvolume is gone.
async fn delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let w = my_ws(&s, &owner, &id).await?;
    let c = kube(&s)?;
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    ws.delete(&id, &DeleteParams::default()).await.map_err(kube_err)?;
    let mut doc = ws_doc(&w, &HashSet::new());
    doc.state = WsState::Deleted;
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}

async fn start_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    my_ws(&s, &owner, &id).await?;
    set_desired::<crd::Workspace>(kube(&s)?, &id, DesiredState::Running).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

async fn stop_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    my_ws(&s, &owner, &id).await?;
    set_desired::<crd::Workspace>(kube(&s)?, &id, DesiredState::Stopped).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(serde::Deserialize)]
struct PackagesBody {
    packages: Vec<String>,
}

/// Change the declared package list. A merge patch on `spec.packages` alone, for the same reason
/// `set_desired` is one: this handler was sent one field and must not claim ownership of a spec
/// the caller never wrote.
async fn patch_ws_packages(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PackagesBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    my_ws(&s, &owner, &id).await?;
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    let patch = serde_json::json!({"spec": {"packages": body.packages}});
    let w = api
        .patch(&id, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    Ok(Json(ws_doc(&w, &HashSet::new())).into_response())
}

#[derive(serde::Deserialize)]
struct CloneBody {
    name: String,
}

/// The one local-copy route.
///
/// It no longer copies a node from the source: locality is the CLAIM's job now, through the
/// source's `status.compatibleNodes`. Copying a node here would be this process authoring a fact it
/// does not own, and it would go stale the moment node retirement moved the source.
async fn clone_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let src = my_ws(&s, &owner, &id).await?;
    let c = kube(&s)?;
    let new_id = rid("ws");
    let volume = ws_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner,
            // A clone lives where its source lives: same team, same namespace.
            team: src.spec.team.clone(),
            name: body.name,
            region: src.spec.region.clone(),
            image: src.spec.image.clone(),
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                source: Some(VolumeSource::CloneOf { volume }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
            resources: Default::default(),
            node_name: None,
            volume_ref: None,
            packages: src.spec.packages.clone(),
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &HashSet::new()))).into_response())
}

/// What a copy of `volume` should be sized at.
///
/// A release-1 object created before `spec.storage` existed carries no quota, and 0 is NOT a
/// "controller default" — `k8s::local_pv`/`claim` format it straight into a `0Gi` PV and PVC. The
/// quota of a legacy source lives on its Volume, which is the object the controller sizes the disk
/// from, so read it there rather than inventing a number.
const FALLBACK_QUOTA_GB: u64 = 20;
async fn storage_quota(c: &kube::Client, storage: &Option<crd::WorkspaceStorage>, volume: &str) -> u64 {
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

/// A workspace whose `Volume` the controller has not reported yet: 409, not a 500 and not a
/// silently dropped request. The caller can retry in a second.
fn not_ready() -> Response {
    (StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response()
}

#[derive(serde::Deserialize)]
struct RestoreBody {
    name: String,
    snapshot_id: String,
    /// Accepted and ignored. The snapshot's own volume is found by looking the id up in the
    /// caller's history, so the client no longer has to know (or still have) a source workspace.
    /// Kept on the wire so a web build from before this change keeps working through a roll.
    #[serde(default)]
    #[allow(dead_code)]
    src_workspace: Option<String>,
}

/// The owner label a snapshot's volume lives under, the volume, and its record — searched across
/// every owner the caller may read. A miss is 404 for "no such snapshot" and "not yours" alike, the same rule the browse
/// tier keeps one tier down.
///
/// Resolved against the SERVER tier's history, not a live workspace: restoring is most useful
/// precisely when the original is gone, and requiring `my_ws(src)` first is what made a deleted
/// workspace's snapshots unrestorable. Shared by the workspace and environment restore routes so
/// the two cannot drift on which snapshots a caller may reach.
///
/// ponytail: serial, one history read per volume until the id is found — an owner with many
/// volumes pays for the ones sorted before theirs. Bound it the way the listing does (buffered 8)
/// if that shows up; the real fix is a snapshot-id -> volume index on the server tier.
async fn find_snapshot(
    s: &ApiState,
    owner: &str,
    snapshot_id: &str,
) -> Result<(String, String, CommitRecord), Response> {
    let up = upstream(s)?;
    for label in caller_owners(s, owner).await {
        let Some(rows) = up.volumes(&label, &label).await.map_err(upstream_err)? else { continue };
        for row in rows {
            let Some(recs) = up.history(&label, &label, &row.name).await.map_err(upstream_err)? else { continue };
            if let Some(rec) = recs.into_iter().find(|r| r.id == snapshot_id) {
                // The LABEL, not the caller: a team's volume lives under the team slug, and the
                // agent has to read it from there. Returning only the caller sent it looking under
                // a label that has no such volume.
                return Ok((label, row.name, rec));
            }
        }
    }
    Err(not_found())
}

/// New workspace grafted onto an explicit, possibly-older snapshot — a PUSHED commit, which is
/// what makes this different from `clone` (always a copy of the current state).
///
/// The snapshot is resolved against the SERVER tier's history, not a live workspace: restoring is
/// most useful precisely when the original is gone, and requiring `my_ws(src)` first is what made
/// a deleted workspace's snapshots unrestorable.
async fn restore_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    let (src_owner, volume, record) = find_snapshot(&s, &owner, &body.snapshot_id).await?;

    // A live source still knows its own size and settings; a deleted one gets the standard quota.
    let src = my_ws(&s, &owner, &volume).await.ok();
    let quota = match &src {
        Some(w) => storage_quota(c, &w.spec.storage, &volume).await,
        // A deleted source cannot be asked its size, and nothing user-facing offers to name one:
        // someone recovering a lost workspace is not sizing a disk. The standard quota, which is
        // also what `create` sends by default.
        None => FALLBACK_QUOTA_GB,
    };
    let new_id = rid("ws");
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner,
            team: src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default(),
            name: body.name,
            // The record knows where its bytes are; a deleted workspace cannot be asked.
            region: src.as_ref().map(|w| w.spec.region.clone()).unwrap_or_else(|| record.region.clone()),
            image: src.as_ref().map(|w| w.spec.image.clone()).unwrap_or_else(default_ws_image),
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                source: Some(VolumeSource::RestoreOf {
                    volume,
                    snapshot_id: body.snapshot_id,
                    // The label the volume was FOUND under, which for a workspace is always the
                    // person's own — set anyway, so the agent never has to assume.
                    owner: Some(src_owner),
                    // The RECORD's region, not the new workspace's: the bytes are wherever they
                    // were pushed, and the node that materializes them has to be told which
                    // container to read. A k3s agent cannot see a VM region's blobs otherwise, and
                    // the fetch that cannot see them never returned an error.
                    region: Some(record.region.clone()),
                }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
            resources: Default::default(),
            node_name: None,
            volume_ref: None,
            packages: vec![],
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &HashSet::new()))).into_response())
}

// ── environments ─────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewEnvironment {
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
    20
}

/// Resolve `NewEnvironment.owner` against the caller: personal (`None` or `caller`) always
/// passes; a different owner must be a team the caller belongs to, which needs a directory —
/// 503 rather than silently creating an environment nobody but this caller can ever see again.
async fn resolve_new_owner(s: &ApiState, caller: &str, owner: Option<String>) -> Result<String, Response> {
    let Some(owner) = owner else { return Ok(caller.to_string()) };
    if owner == caller {
        return Ok(owner);
    }
    match &s.membership {
        None => Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response()),
        Some(_) if may_act_on(s, caller, &owner).await => Ok(owner),
        Some(_) => Err((StatusCode::FORBIDDEN, "not a member of that team").into_response()),
    }
}

/// Finds an environment by id and authorizes the caller against its owner: their own always
/// passes, a team's passes when they are a member. An environment they may not act on is a 404,
/// never a 403 — the caller learns nothing about environments that are not theirs.
async fn find_env(s: &ApiState, caller: &str, id: &str) -> Result<crd::Environment, Response> {
    let api: Api<crd::Environment> = Api::all(kube(s)?.clone());
    let e = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if !may_act_on(s, caller, &e.spec.owner).await {
        return Err(not_found());
    }
    Ok(e)
}

/// The trust boundary for mounts: this is the only route that accepts caller-authored services
/// (`clone_env` copies an already-validated doc, and nothing updates services in place), so a
/// mount that gets past here is treated as trusted by a root agent from then on.
fn check_mounts(services: &[Service]) -> Result<(), String> {
    services.iter().flat_map(|s| &s.mounts).try_for_each(crate::model::validate_mount)
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

async fn create_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewEnvironment>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    // Mounts name volumes (folders inside the env's own subvolume), not workspaces. The name is
    // joined onto the env's subvolume by a root agent, so it is a security boundary, not a
    // formality — see `validate_mount`. Checked before anything is written, deliberately.
    if let Err(e) = check_mounts(&body.services) {
        return Err((StatusCode::BAD_REQUEST, e).into_response());
    }
    let owner = resolve_new_owner(&s, &caller_id, body.owner).await?;
    let c = kube(&s)?;
    let id = rid("env");
    let e = create_environment(
        c,
        &id,
        crd::EnvironmentSpec {
            owner,
            name: body.name,
            region: body.region,
            services: body.services,
            storage: Some(crd::WorkspaceStorage { quota_gb: body.quota_gb, source: None }),
            desired_state: DesiredState::Running,
            restore: None,
            node_name: None,
            volume_ref: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

#[derive(serde::Deserialize)]
struct RestoreEnvBody {
    name: String,
    snapshot_id: String,
    /// A team slug, resolved exactly as `NewEnvironment.owner` is — restoring a team's snapshot
    /// must produce a TEAM environment, or the restored copy is invisible to everyone but the
    /// person who clicked.
    #[serde(default)]
    owner: Option<String>,
    /// Validated exactly as `create_env`'s are — `check_mounts` is the trust boundary for mounts
    /// and a restore is just as much a caller-authored service list as a create is.
    #[serde(default)]
    services: Vec<Service>,
    /// The region to RUN in. Where the snapshot's bytes live is the record's business, not this
    /// field's — that goes on the volume source.
    #[serde(default)]
    region: Option<String>,
    #[serde(default = "default_env_quota")]
    quota_gb: u64,
}

/// New environment grafted onto an explicit past snapshot — `restore_ws`'s twin, resolving the
/// snapshot the same way (server-tier history, caller/team scoping) and differing only in which
/// kind of object it writes. The agent needs no new path: `resolve_volume` already materializes a
/// `restoreOf` source for an Environment.
///
/// The services are the caller's, because a snapshot does not record them: the commit record
/// carries provenance (what the volume was OF), not a compose file. Restoring with none is legal
/// and gives back the DATA — which is the thing that could not be reconstructed.
async fn restore_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreEnvBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    if let Err(e) = check_mounts(&body.services) {
        return Err((StatusCode::BAD_REQUEST, e).into_response());
    }
    // Named before anything is written, like `create_env`'s: an environment with no name is a row
    // nobody can tell apart from another.
    if body.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required").into_response());
    }
    let (src_owner, volume, record) = find_snapshot(&s, &caller_id, &body.snapshot_id).await?;
    // Defaults to the label the snapshot was FOUND under, not the caller: restoring a team's
    // environment produces a team environment without the client having to say so.
    let owner = resolve_new_owner(&s, &caller_id, body.owner.or(Some(src_owner.clone()))).await?;
    // The record's own services, when the caller named none: an environment's push writes them into
    // its provenance precisely so a restore of a DELETED environment can bring it back running. A
    // caller-supplied list wins (and is validated above); a record without one restores the data
    // and no services, which the UI says out loud.
    let services = match body.services.is_empty() {
        false => body.services,
        true => crate::upstream::Provenance::of(&record.state).services.unwrap_or_default(),
    };
    let c = kube(&s)?;
    let id = rid("env");
    let e = create_environment(
        c,
        &id,
        crd::EnvironmentSpec {
            owner,
            name: body.name,
            // Unspecified means "where the bytes already are", which is the one region guaranteed
            // to exist for this snapshot.
            region: body.region.unwrap_or_else(|| record.region.clone()),
            services,
            storage: Some(crd::WorkspaceStorage {
                quota_gb: body.quota_gb,
                source: Some(VolumeSource::RestoreOf {
                    volume,
                    snapshot_id: body.snapshot_id,
                    owner: Some(src_owner),
                    region: Some(record.region.clone()),
                }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
            node_name: None,
            volume_ref: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

#[derive(serde::Deserialize)]
struct ListEnvQuery {
    /// Filter to one owner (a username or a team slug) — what the web app's `/{owner}/environments`
    /// page passes so a team page shows only that team's environments, not the caller's personal
    /// ones mixed in. Validated the same way `create_env`'s team owner is: caller must be that
    /// owner, or a member of it.
    #[serde(default)]
    owner: Option<String>,
}

async fn list_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListEnvQuery>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let owners: Vec<String> = match q.owner {
        Some(o) if may_act_on(&s, &caller_id, &o).await => vec![o],
        Some(_) => return Err(not_found()),
        None => {
            let mut owners = vec![caller_id.clone()];
            owners.extend(teams_for(&s, &caller_id).await);
            owners
        }
    };
    let c = kube(&s)?;
    let api: Api<crd::Environment> = Api::all(c.clone());
    let mut list = vec![];
    for owner in owners {
        let pushed = pushed_volumes(c, &owner).await?;
        for e in api.list(&owned_by(&owner)).await.map_err(kube_err)?.items {
            list.push(env_doc(&e, &pushed));
        }
    }
    Ok(Json(list).into_response())
}

async fn get_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let pushed = pushed_volumes(c, &e.spec.owner).await?;
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

async fn start_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Running).await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

async fn stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Stopped).await?;
    let mut doc = env_doc(&e, &HashSet::new());
    doc.state = EnvState::Stopped;
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}

async fn delete_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let envs: Api<crd::Environment> = Api::all(c.clone());
    envs.delete(&id, &DeleteParams::default()).await.map_err(kube_err)?;
    let mut doc = env_doc(&e, &HashSet::new());
    doc.state = EnvState::Deleted;
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}

/// Env's local-copy route. Names no node, for the same reason `clone_ws` does not, and the
/// source's already-validated services carry over untouched.
async fn clone_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let src = find_env(&s, &caller_id, &id).await?;
    let c = kube(&s)?;
    let new_id = rid("env");
    let volume = env_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
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
                source: Some(VolumeSource::CloneOf { volume }),
            }),
            desired_state: DesiredState::Running,
            restore: None,
            node_name: None,
            volume_ref: None,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

// ── push ─────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct PushBody {
    message: Option<String>,
}

/// The push body is optional (`{message?}`), and axum's `Json<T>` extractor 415s a request
/// with no body/content-type at all rather than treating it as absent — so the message is read
/// as raw bytes and parsed only when present, same forgiving shape a curl with no `-d` expects.
async fn optional_push_message(body: axum::body::Bytes) -> Result<Option<String>, Response> {
    if body.is_empty() {
        return Ok(None);
    }
    let parsed: PushBody = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid push body").into_response())?;
    Ok(parsed.message)
}

/// A push is an OBJECT, not an annotation: a wish with an OUTCOME needs somewhere to put the
/// outcome, which is what the annotation this replaces did not have.
///
/// The spec is `{volume, message?}` and nothing else. A node is a controller-owned fact: copying it
/// here would go stale the moment node retirement moved the Volume. The agent resolves the node
/// from the named Volume — every agent watches every request and acts only on its own.
///
/// The Volume still has to EXIST, though: a push against a workspace whose disk has not been made
/// yet is a 409 the user can act on, not a request that sits pending forever.
async fn request_snapshot(
    c: &kube::Client,
    volume: Option<&str>,
    message: Option<String>,
) -> Result<Response, Response> {
    let Some(volume) = volume else { return Err(not_ready()) };
    let vols: Api<crd::Volume> = Api::all(c.clone());
    // The owner comes off the Volume, never off the caller: `spec.owner` is the truth and the
    // request's label is only a view of it.
    let Some(owner) = vols.get_opt(volume).await.map_err(kube_err)?.map(|v| v.spec.owner) else {
        return Err(not_ready());
    };
    let name = rid("snap");
    let req = crd::snapshot_request(&name, &owner, volume, message);
    let api: Api<crd::SnapshotRequest> = Api::all(c.clone());
    api.create(&PostParams::default(), &req).await.map_err(kube_err)?;
    // The name, so a client can follow ONE push instead of polling the volume's whole history.
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id": name}))).into_response())
}

async fn push_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let w = my_ws(&s, &owner, &id).await?;
    let msg = optional_push_message(body).await?;
    request_snapshot(kube(&s)?, ws_volume(&w), msg).await
}

async fn push_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    let msg = optional_push_message(body).await?;
    request_snapshot(kube(&s)?, env_volume(&e), msg).await
}

#[derive(serde::Deserialize)]
struct RestoreInPlaceBody {
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
async fn restore_env_in_place(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RestoreInPlaceBody>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    let (src_owner, volume, record) = find_snapshot(&s, &caller_id, &body.snapshot_id).await?;
    let wish = crd::RestoreWish {
        snapshot_id: body.snapshot_id,
        volume,
        owner: Some(src_owner),
        region: Some(record.region.clone()),
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
// around?", which is a display detail, never an authorization one. `SnapshotRequest` is the push
// WORK ITEM and nothing here reads it — a request that has been garbage-collected costs nothing.

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
    /// Epoch millis of the volume's last write. Approximate by construction — see the
    /// `volumes` handler on the server tier.
    latest_ms: Option<i64>,
}

fn upstream(s: &ApiState) -> Result<&Arc<crate::upstream::Upstream>, Response> {
    s.upstream
        .as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "registry upstream not configured").into_response())
}

fn upstream_err(e: String) -> Response {
    tracing::error!(error = %e, "volume upstream");
    (StatusCode::BAD_GATEWAY, "registry unavailable").into_response()
}

/// Every owner label the caller may read volumes under: themselves, plus each team they belong to
/// (team-owned environments). Membership is verified HERE — the server tier trusts whatever owner
/// this tier names in `OWNER_HEADER`, so an unverified value would be a data leak.
async fn caller_owners(s: &ApiState, owner: &str) -> Vec<String> {
    let mut v = vec![owner.to_string()];
    v.extend(teams_for(s, owner).await);
    v
}

/// The live parents, by volume id, with the kind they are. One list call per kind, never one per
/// row — and used ONLY for `deleted` and as a provenance fallback.
///
/// `None` means the cluster could not be asked, which is NOT the same as "nothing is alive": the
/// difference decides whether every row on the page is labelled "source deleted" during a kube
/// blip. The caller keeps `deleted: false` on `None` — the snapshots are what the page is for, and
/// they are all still there.
async fn live_parents(s: &ApiState, owner: &str, owners: &[String]) -> Option<BTreeMap<String, (String, String)>> {
    let c = s.kube.as_ref()?;
    let mut live = BTreeMap::new();
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    for w in ws.list(&owned_by(owner)).await.ok()?.items {
        live.insert(w.name_any(), ("workspace".to_string(), w.spec.name.clone()));
    }
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let lp = ListParams::default().labels(&format!("{OWNER_LABEL} in ({})", owners.join(",")));
    for e in envs.list(&lp).await.ok()?.items {
        live.insert(e.name_any(), ("environment".to_string(), e.spec.name.clone()));
    }
    Some(live)
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
struct ListVolQuery {
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

async fn list_volumes(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListVolQuery>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let up = upstream(&s)?;
    let owners = match &q.owner {
        Some(o) if may_act_on(&s, &caller_id, o).await => vec![o.clone()],
        Some(_) => return Err(not_found()),
        None => caller_owners(&s, &caller_id).await,
    };

    // The cluster answers only "does a parent still exist", so a kube outage degrades the page to
    // bare ids rather than emptying it — the snapshots themselves do not live there. `None` is an
    // unanswered question, never an answer of "nothing": labelling every row "source deleted"
    // during a blip, and then fanning out a history read per row to name them, is the failure mode
    // this distinction exists to prevent.
    // `owners[0]` rather than the caller: with an `owner` filter this is the team being listed, and
    // asking for the caller's own workspaces would name rows that are not on this page.
    let live = live_parents(&s, owners.first().map(String::as_str).unwrap_or(&caller_id), &owners).await;
    let known = live.is_some();
    let live = live.unwrap_or_default();

    let mut out: Vec<VolumeSummary> = vec![];
    for owner in &owners {
        let Some(rows) = up.volumes(owner, owner).await.map_err(upstream_err)? else { continue };
        for row in rows {
            let parent = live.get(&row.name);
            out.push(VolumeSummary {
                kind: parent.map(|(k, _)| k.clone()).unwrap_or_default(),
                display_name: parent.map(|(_, n)| n.clone()).unwrap_or_default(),
                deleted: known && parent.is_none(),
                volume: Some(format!("vol/{owner}/{}", row.name)),
                latest_ms: row.latest_ms,
                name: row.name,
            });
        }
    }

    // Provenance for the rows a live parent could not name — the deleted ones, which is exactly the
    // case this whole listing exists for. One history read each, and only for those.
    // ponytail: N reads for N deleted volumes, bounded at 8 in flight. The upgrade is provenance in
    // the listing itself, which needs a per-push marker under `index/` since the listing handler
    // may never open a volume database.
    let jobs: Vec<(String, String, bool)> = out
        .iter()
        .map(|v| {
            let owner = v.volume.as_deref().unwrap_or_default().split('/').nth(1).unwrap_or_default().to_string();
            (owner, v.name.clone(), v.deleted)
        })
        .collect();
    // `Some(None)` is a deleted volume whose history came back EMPTY: a database that exists
    // because something opened it (an aborted first push, a probe) but never received a commit.
    // It has no snapshot to show or restore, so it is dropped from the listing rather than
    // shown as a ghost "source deleted" row with nothing behind it.
    let named: Vec<Option<Option<crate::upstream::Provenance>>> = futures::stream::iter(jobs)
        .map(|(owner, name, deleted)| {
            let up = up.clone();
            async move {
                if !deleted {
                    return None;
                }
                let recs = up.history(&owner, &owner, &name).await.ok()??;
                Some(recs.first().map(|r| crate::upstream::Provenance::of(&r.state)))
            }
        })
        .buffered(8)
        .collect()
        .await;

    let mut keep = Vec::with_capacity(out.len());
    for (mut v, p) in out.into_iter().zip(named) {
        if let Some(None) = p {
            continue;
        }
        if let Some(Some(p)) = p {
            if let Some(k) = p.kind {
                v.kind = k;
            }
            if let Some(n) = p.name {
                v.display_name = n;
            }
        }
        if v.kind.is_empty() {
            v.kind = kind_of(&v.name);
        }
        if v.display_name.is_empty() {
            v.display_name = v.name.clone();
        }
        keep.push(v);
    }
    // Filtered here, after provenance and the id-prefix fallback have decided what each row IS —
    // filtering earlier would drop rows on the empty kind they start with.
    if let Some(kind) = &q.kind {
        keep.retain(|v| &v.kind == kind);
    }
    keep.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(keep).into_response())
}

/// The owner label a volume is readable under, or 404. Ownership is the SERVER tier's answer: it
/// refuses a volume that is not the named owner's, and this tier only decides which owners the
/// caller may ask as. No live parent is required — that is the whole fix.
async fn volume_owner(s: &ApiState, caller_id: &str, name: &str) -> Result<(String, Vec<CommitRecord>), Response> {
    let up = upstream(s)?;
    for owner in caller_owners(s, caller_id).await {
        if let Some(recs) = up.history(&owner, &owner, name).await.map_err(upstream_err)? {
            if !recs.is_empty() {
                return Ok((owner, recs));
            }
        }
    }
    Err(not_found())
}

/// `DELETE /v1/volumes/{name}` — drop a volume's snapshots. What the environment Delete dialog
/// calls when "Also delete its snapshots" is checked, and what an archived row's "Delete
/// snapshots" calls on its own.
///
/// Scoped exactly like `history`: `volume_owner` decides which owner label the caller may read it
/// under, and a volume that is not theirs is a 404 rather than a 403 — they learn nothing about
/// volumes that are not theirs.
async fn delete_volume(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (owner, _) = volume_owner(&s, &caller_id, &name).await?;
    match upstream(&s)?.delete_volume(&owner, &owner, &name).await.map_err(upstream_err)? {
        true => Ok(StatusCode::NO_CONTENT.into_response()),
        false => Err(not_found()),
    }
}

/// `DELETE /v1/volumes/{name}/snapshots/{snapshot}` — drop ONE snapshot record from the lineage.
///
/// Scoped exactly like `history`, and 404 for the same two cases the server tier collapses: a
/// volume the caller may not read, and a snapshot id that is not in it.
async fn delete_snapshot(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((name, snapshot)): Path<(String, String)>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (owner, _) = volume_owner(&s, &caller_id, &name).await?;
    match upstream(&s)?.delete_snapshot(&owner, &owner, &name, &snapshot).await.map_err(upstream_err)? {
        true => Ok(StatusCode::NO_CONTENT.into_response()),
        false => Err(not_found()),
    }
}

async fn volume_history(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (_, records) = volume_owner(&s, &caller_id, &name).await?;
    // A record carries its whole blob chain and never names another record, so "parent" is
    // derived: the record whose chain is this one's chain minus its last blob. An in-place restore
    // followed by a push grafts onto the RESTORED record, which is what makes the history a tree
    // rather than a list — and the page can only draw the branch if the row says where it forks.
    fn chain(r: &crate::registry::CommitRecord) -> Vec<&str> {
        r.lineage.iter().map(|e| e.blob.as_str()).collect()
    }
    let rows: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
            let mine = chain(r);
            let parent = records
                .iter()
                .find(|p| p.id != r.id && chain(p) == mine[..mine.len().saturating_sub(1)])
                .map(|p| p.id.clone());
            let mut v = serde_json::to_value(r).unwrap_or_default();
            v["parent"] = serde_json::json!(parent);
            v
        })
        .collect();
    Ok(Json(rows).into_response())
}

/// There is exactly one ref per volume ("main") and its value is always the newest snapshot — the
/// same "first = tip" convention `engine::ops` relies on.
async fn volume_refs(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let (_, records) = volume_owner(&s, &caller_id, &name).await?;
    let tip = records.first().map(|r| r.id.clone());
    Ok(Json(serde_json::json!({crate::registry_client::MAIN_REF: tip})).into_response())
}

#[cfg(test)]
mod tests {
    use super::{check_mounts, ws_doc};
    use crate::crd;
    use crate::model::{Mount, Service};

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
                restore: None,
                resources: Default::default(),
                node_name: None,
                volume_ref: None,
                packages: vec![],
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
        assert!(check_mounts(&[svc("data", "/data")]).is_ok());
        // The C1 payload: `{"folder": "/", "path": "/host"}` bind-mounts the host root RW into a
        // container whose image the same caller chose.
        for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
            assert!(check_mounts(&[svc(bad, "/host")]).is_err(), "folder {bad:?} must be refused");
        }
        assert!(check_mounts(&[svc("data", "/data:/etc")]).is_err(), "a ':' in path splices a mapping");
        assert!(check_mounts(&[svc("data", "relative")]).is_err());
    }
}
