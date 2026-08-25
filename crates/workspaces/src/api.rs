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

use crate::crd::{self, DesiredState, VolumeSource, VolumeSpec};
use crate::model::*;
use crate::registry_client::{MAIN_REF, RegistryClient};
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
    /// Reads volume history/refs by calling the server tier's `/vol-agent/{owner}/{name}/*`
    /// surface (`bins/server/src/vol_agent.rs`) with a shared agent token — the same
    /// `RegistryClient` the agent binary already uses to WRITE that surface, reused here to
    /// READ it. Picked over building a peer-listener forward (the `crates/api` browse pattern):
    /// this process has no peer secret or ownership-routing plumbing at all today, while
    /// `RegistryClient` already exists, is tested, and the vol-agent routes are public-listener
    /// and token-gated by design (agents are never on the peer network either). `None` when
    /// unconfigured — volume routes answer 503 rather than not existing.
    pub registry: Option<RegistryClient>,
    /// Team lookups for team-owned environments (see module docs' Part 2). `None` means no
    /// directory is wired (dev, or the directory tier is down) — team envs answer 503 rather than
    /// silently behaving as if the caller has no teams.
    pub membership: Option<Arc<dyn MembershipCheck>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace, environment and
    /// volume route answers 503 rather than not existing — the same shape `registry: None` has.
    pub kube: Option<kube::Client>,
}

impl ApiState {
    pub fn new(store: Arc<dyn MetaStore>, jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState { store, jwt, admins, registry: None, membership: None, kube: None }
    }

    pub fn with_registry(mut self, registry: RegistryClient) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn with_membership(mut self, membership: Arc<dyn MembershipCheck>) -> Self {
        self.membership = Some(membership);
        self
    }

    pub fn with_kube(mut self, client: kube::Client) -> Self {
        self.kube = Some(client);
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
        .route("/v1/workspaces", post(create_ws).get(list_ws))
        .route("/v1/workspaces/restore", post(restore_ws))
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/environments", post(create_env).get(list_env))
        .route("/v1/environments/{id}", get(get_env).delete(delete_env))
        .route("/v1/environments/{id}/start", post(start_env))
        .route("/v1/environments/{id}/stop", post(stop_env))
        .route("/v1/environments/{id}/clone", post(clone_env))
        .route("/v1/environments/{id}/push", post(push_env))
        .route("/v1/volumes", get(list_volumes))
        .route("/v1/volumes/{name}/history", get(volume_history))
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
        status: "active".into(),
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
/// The generation bump a push is. Written on the `Volume`, because the subvolume is what gets
/// pushed — the workspace or environment around it is not involved.
pub const PUSH_ANNOTATION: &str = "rustic-git.io/push-requested";
pub const PUSH_MESSAGE_ANNOTATION: &str = "rustic-git.io/push-message";

/// A label selector is the list filter, not a field selector: `metadata.labels` is indexed for
/// selectors by every API server, while an arbitrary spec field needs a `selectableFields` entry —
/// and adding one per query axis is how a CRD becomes a database.
fn owned_by(owner: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"))
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

/// The registry pointer the web app reads as "has this ever been pushed". On the CRD side that
/// fact lives in the Volume's status, so a doc projection joins the two objects.
fn volume_ptr(owner: &str, id: &str, v: Option<&crd::Volume>) -> Option<String> {
    v.and_then(|v| v.status.as_ref())
        .and_then(|st| st.last_push.as_ref())
        .map(|_| format!("vol/{owner}/{id}"))
}

fn ws_doc(w: &crd::Workspace, v: Option<&crd::Volume>) -> Workspace {
    let id = w.name_any();
    Workspace {
        owner: w.spec.owner.clone(),
        name: w.spec.name.clone(),
        region: w.spec.region.clone(),
        state: phase(w.status.as_ref().map(|s| s.phase.as_str()), WsState::Creating),
        image: w.spec.image.clone(),
        placement: Some(w.spec.node_name.clone()),
        volume: volume_ptr(&w.spec.owner, &id, v),
        quota_gb: v.map(|v| v.spec.quota_gb).unwrap_or(0),
        // Free-form live state was a job-era field the agent wrote back into the doc; the pod and
        // its status are the live state now. Kept in the body so the web app's parse is unchanged.
        live_state: serde_json::Value::Null,
        id,
    }
}

fn env_doc(e: &crd::Environment, v: Option<&crd::Volume>) -> Environment {
    let id = e.name_any();
    Environment {
        owner: e.spec.owner.clone(),
        name: e.spec.name.clone(),
        region: e.spec.region.clone(),
        state: phase(e.status.as_ref().map(|s| s.phase.as_str()), EnvState::Creating),
        placement: Some(e.spec.node_name.clone()),
        volume: volume_ptr(&e.spec.owner, &id, v),
        services: e.spec.services.clone(),
        id,
    }
}

/// Every `Volume` an owner has, keyed by id, so a listing joins in one extra call instead of one
/// per row.
async fn volumes_of(c: &kube::Client, owner: &str) -> Result<BTreeMap<String, crd::Volume>, Response> {
    let api: Api<crd::Volume> = Api::all(c.clone());
    let items = api.list(&owned_by(owner)).await.map_err(kube_err)?.items;
    Ok(items.into_iter().map(|v| (v.name_any(), v)).collect())
}

/// The one place a node is named for a new object. `role` selects the node pool: workspace pods
/// run on `session` nodes, environment workloads on `env` ones — the same two roles `k8s.rs`
/// stamps as `nodeSelector`.
async fn place_node(c: &kube::Client, region: &str, owner: &str, role: &str) -> Result<String, Response> {
    crate::placement::place(c, region, owner, role)
        .await
        .map_err(kube_err)?
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no node available in that region").into_response())
}

async fn create_volume(c: &kube::Client, id: &str, kind: &str, spec: VolumeSpec) -> Result<crd::Volume, Response> {
    let mut vol = crd::Volume::new(id, spec);
    let owner = vol.spec.owner.clone();
    vol.metadata.labels = Some(labels(&owner, kind));
    let api: Api<crd::Volume> = Api::all(c.clone());
    api.create(&PostParams::default(), &vol).await.map_err(kube_err)
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
    name: String,
    region: String,
    quota_gb: u64,
    #[serde(default = "default_ws_image")]
    image: String,
}

/// The `Volume` decides placement and the `Workspace` READS it back from what the API server
/// actually stored — never chooses a node a second time. Two places allowed to name a node is two
/// places that can disagree about where the data is (audit H1).
async fn workspace_for(
    c: &kube::Client,
    id: &str,
    vol: &crd::Volume,
    name: String,
    image: String,
    desired: DesiredState,
) -> Result<crd::Workspace, Response> {
    let mut w = crd::Workspace::new(
        id,
        crd::WorkspaceSpec {
            owner: vol.spec.owner.clone(),
            name,
            region: vol.spec.region.clone(),
            image,
            volume_ref: vol.name_any(),
            node_name: vol.spec.node_name.clone(),
            desired_state: desired,
            resources: Default::default(),
        },
    );
    w.metadata.labels = Some(labels(&vol.spec.owner, "workspace"));
    let api: Api<crd::Workspace> = Api::all(c.clone());
    api.create(&PostParams::default(), &w).await.map_err(kube_err)
}

async fn create_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewWorkspace>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    let id = rid("ws");
    let node = place_node(c, &body.region, &owner, "session").await?;
    let spec = VolumeSpec {
        owner: owner.clone(),
        node_name: node,
        region: body.region,
        quota_gb: body.quota_gb,
        source: None,
    };
    let vol = create_volume(c, &id, "workspace", spec).await?;
    let w = workspace_for(c, &id, &vol, body.name, body.image, DesiredState::Running).await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, Some(&vol)))).into_response())
}

async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    // No "filter out the deleted ones": a deleted object is gone from the API server.
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let items = api.list(&owned_by(&owner)).await.map_err(kube_err)?.items;
    let vols = volumes_of(c, &owner).await?;
    let list: Vec<_> = items.iter().map(|w| ws_doc(w, vols.get(&w.spec.volume_ref))).collect();
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
    let vol: Api<crd::Volume> = Api::all(kube(&s)?.clone());
    let v = vol.get_opt(&w.spec.volume_ref).await.map_err(kube_err)?;
    Ok(Json(ws_doc(&w, v.as_ref())).into_response())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// The `Workspace` goes first and the `Volume` second: the pod has to be gone before the
/// subvolume under it can be reclaimed, and the `Volume`'s finalizer is what holds that order.
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
    let vol: Api<crd::Volume> = Api::all(c.clone());
    vol.delete(&w.spec.volume_ref, &DeleteParams::default()).await.map_err(kube_err)?;
    let mut doc = ws_doc(&w, None);
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
struct CloneBody {
    name: String,
}

/// The one local-copy route. The copy lands on the SOURCE's node, not on a freshly picked one: a
/// clone is a local btrfs snapshot, and a node chosen independently would turn it into a network
/// copy of data that is already here.
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
    let src_vol: Api<crd::Volume> = Api::all(c.clone());
    let src_vol = src_vol.get(&src.spec.volume_ref).await.map_err(kube_err)?;
    let spec = VolumeSpec {
        owner,
        node_name: src.spec.node_name.clone(),
        region: src.spec.region.clone(),
        quota_gb: src_vol.spec.quota_gb,
        source: Some(VolumeSource::CloneOf { volume: src.spec.volume_ref.clone() }),
    };
    let vol = create_volume(c, &new_id, "workspace", spec).await?;
    let w =
        workspace_for(c, &new_id, &vol, body.name, src.spec.image.clone(), DesiredState::Running).await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, Some(&vol)))).into_response())
}

#[derive(serde::Deserialize)]
struct RestoreBody {
    name: String,
    snapshot_id: String,
    src_workspace: String,
}

/// New workspace grafted onto an explicit, possibly-older snapshot — a PUSHED commit, which is
/// what makes this different from `clone` (always a copy of the current state).
async fn restore_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let src = my_ws(&s, &owner, &body.src_workspace).await?;
    // Snapshot records live in the VOLUME REGISTRY — Cosmos's `snapshots` container is a dead
    // keyspace nothing writes, and validating against it 404'd every restore (caught by the live
    // e2e, invisible to unit tests that seeded Cosmos-style).
    let snap = registry(&s)?
        .get_history(&owner, &body.src_workspace)
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "volume registry unreachable").into_response())?
        .into_iter()
        .find(|r| r.id == body.snapshot_id)
        .ok_or_else(not_found)?;
    let c = kube(&s)?;
    let new_id = rid("ws");
    let src_vol: Api<crd::Volume> = Api::all(c.clone());
    let quota = src_vol.get(&src.spec.volume_ref).await.map_err(kube_err)?.spec.quota_gb;
    let node = place_node(c, &src.spec.region, &owner, "session").await?;
    let spec = VolumeSpec {
        owner,
        node_name: node,
        region: src.spec.region.clone(),
        quota_gb: quota,
        source: Some(VolumeSource::RestoreOf { volume: src.spec.volume_ref.clone(), snapshot_id: snap.id }),
    };
    let vol = create_volume(c, &new_id, "workspace", spec).await?;
    let w =
        workspace_for(c, &new_id, &vol, body.name, src.spec.image.clone(), DesiredState::Running).await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, Some(&vol)))).into_response())
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

async fn environment_for(
    c: &kube::Client,
    id: &str,
    vol: &crd::Volume,
    name: String,
    services: Vec<Service>,
) -> Result<crd::Environment, Response> {
    let mut e = crd::Environment::new(
        id,
        crd::EnvironmentSpec {
            owner: vol.spec.owner.clone(),
            name,
            region: vol.spec.region.clone(),
            services,
            volume_ref: vol.name_any(),
            // Read back from the Volume, same rule as `workspace_for`.
            node_name: vol.spec.node_name.clone(),
            desired_state: DesiredState::Running,
        },
    );
    e.metadata.labels = Some(labels(&vol.spec.owner, "environment"));
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
    let node = place_node(c, &body.region, &owner, "env").await?;
    let spec = VolumeSpec {
        owner,
        node_name: node,
        region: body.region,
        quota_gb: body.quota_gb,
        source: None,
    };
    let vol = create_volume(c, &id, "environment", spec).await?;
    let e = environment_for(c, &id, &vol, body.name, body.services).await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, Some(&vol)))).into_response())
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
        let vols = volumes_of(c, &owner).await?;
        for e in api.list(&owned_by(&owner)).await.map_err(kube_err)?.items {
            let v = vols.get(&e.spec.volume_ref);
            list.push(env_doc(&e, v));
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
    let vol: Api<crd::Volume> = Api::all(kube(&s)?.clone());
    let v = vol.get_opt(&e.spec.volume_ref).await.map_err(kube_err)?;
    Ok(Json(env_doc(&e, v.as_ref())).into_response())
}

async fn start_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Running).await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, None))).into_response())
}

async fn stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers)?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Stopped).await?;
    let mut doc = env_doc(&e, None);
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
    let vol: Api<crd::Volume> = Api::all(c.clone());
    vol.delete(&e.spec.volume_ref, &DeleteParams::default()).await.map_err(kube_err)?;
    let mut doc = env_doc(&e, None);
    doc.state = EnvState::Deleted;
    Ok((StatusCode::ACCEPTED, Json(doc)).into_response())
}

/// Env's local-copy route. Same node as the source for the same reason `clone_ws` uses it, and the
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
    let src_vol: Api<crd::Volume> = Api::all(c.clone());
    let quota = src_vol.get(&src.spec.volume_ref).await.map_err(kube_err)?.spec.quota_gb;
    let spec = VolumeSpec {
        owner: src.spec.owner.clone(),
        node_name: src.spec.node_name.clone(),
        region: src.spec.region.clone(),
        quota_gb: quota,
        source: Some(VolumeSource::CloneOf { volume: src.spec.volume_ref.clone() }),
    };
    let vol = create_volume(c, &new_id, "environment", spec).await?;
    let e = environment_for(c, &new_id, &vol, body.name, src.spec.services.clone()).await?;
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, Some(&vol)))).into_response())
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

/// Push stays the one mutating verb; what changed is that the OBJECT is the work item. Stamping a
/// timestamp annotation on the `Volume` is a generation bump the controller converges toward —
/// there is nothing to enqueue, and a second push while one is running is the same annotation
/// moving forward rather than a duplicate job.
async fn request_push(c: &kube::Client, volume: &str, message: Option<String>) -> Result<Response, Response> {
    let mut ann = serde_json::json!({PUSH_ANNOTATION: chrono::Utc::now().to_rfc3339()});
    if let Some(m) = message {
        ann[PUSH_MESSAGE_ANNOTATION] = serde_json::json!(m);
    }
    let api: Api<crd::Volume> = Api::all(c.clone());
    let patch = serde_json::json!({"metadata": {"annotations": ann}});
    api.patch(volume, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
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
    request_push(kube(&s)?, &w.spec.volume_ref, msg).await
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
    request_push(kube(&s)?, &e.spec.volume_ref, msg).await
}

// ── volumes ──────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct VolumeSummary {
    /// Registry name — the ws/env id, matching the `{owner}/{name}` the vol-agent surface and
    /// `RegistryClient` already key on.
    name: String,
    kind: String,
    /// `None` until the workspace/environment's first push writes a volume pointer.
    volume: Option<String>,
}

async fn list_volumes(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    let c = kube(&s)?;
    let mut out = vec![];
    for (id, v) in volumes_of(c, &owner).await? {
        let kind = v.labels().get(KIND_LABEL).cloned().unwrap_or_default();
        out.push(VolumeSummary { volume: volume_ptr(&owner, &id, Some(&v)), name: id, kind });
    }
    // Environments can be team-owned; workspaces stay strictly personal, so a team's listing is
    // narrowed to environment volumes by the same label that names their kind.
    for team in teams_for(&s, &owner).await {
        let api: Api<crd::Volume> = Api::all(c.clone());
        let lp = ListParams::default().labels(&format!("{OWNER_LABEL}={team},{KIND_LABEL}=environment"));
        for v in api.list(&lp).await.map_err(kube_err)?.items {
            let id = v.name_any();
            out.push(VolumeSummary {
                volume: volume_ptr(&team, &id, Some(&v)),
                name: id,
                kind: "environment".into(),
            });
        }
    }
    Ok(Json(out).into_response())
}

fn registry(s: &ApiState) -> Result<&RegistryClient, Response> {
    s.registry.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "volume registry not configured on this node").into_response()
    })
}

/// A volume `name` is only readable by the caller who owns the workspace or environment it
/// belongs to — the registry itself has no owner check (it trusts the agent token, not a JWT),
/// so this crate enforces it before ever asking the registry for anything.
async fn owns_volume(s: &ApiState, owner: &str, name: &str) -> Result<(), Response> {
    let c = kube(s)?;
    // Workspaces stay strictly personal.
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    if ws.get_opt(name).await.map_err(kube_err)?.is_some_and(|w| w.spec.owner == owner) {
        return Ok(());
    }
    // Environments can be team-owned: the caller's own, then each team they belong to.
    let envs: Api<crd::Environment> = Api::all(c.clone());
    if let Some(e) = envs.get_opt(name).await.map_err(kube_err)? {
        if may_act_on(s, owner, &e.spec.owner).await {
            return Ok(());
        }
    }
    Err(not_found())
}

async fn volume_history(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    owns_volume(&s, &owner, &name).await?;
    let reg = registry(&s)?;
    let history = reg
        .get_history(&owner, &name)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e).into_response())?;
    Ok(Json(history).into_response())
}

/// Derived from `history` rather than a raw registry ref-read: the vol-agent surface exposes no
/// `GET .../ref` (only the agent-only `POST .../ref` that moves it — see
/// `bins/server/src/vol_agent.rs`'s `VOL_AGENT_TAILS`), and there is exactly one ref per volume
/// (`MAIN_REF`, "main") whose value is always the newest commit in `history` — the same
/// "first = tip" convention `engine::ops` already relies on. Cheaper than adding a new
/// agent-token-gated route for one more read of data `history` already carries.
async fn volume_refs(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers)?;
    owns_volume(&s, &owner, &name).await?;
    let reg = registry(&s)?;
    let history = reg
        .get_history(&owner, &name)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e).into_response())?;
    let tip = history.first().map(|r| r.id.clone());
    Ok(Json(serde_json::json!({MAIN_REF: tip})).into_response())
}

#[cfg(test)]
mod tests {
    use super::check_mounts;
    use crate::model::{Mount, Service};

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
