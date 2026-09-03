//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation writes a CUSTOM RESOURCE and answers 202 with a projection of it. The object is
//! the work item: there is no queue, no lease and no dispatch — the node named by `spec.nodeName`
//! reconciles what it owns. `Region` is a CRD too (`crd::Region`) — cross-cluster metadata by
//! nature, but registered rarely enough that the cluster this tier already talks to is the
//! cheapest correct home for it.
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
use crate::k8s::labels;
use crate::model::*;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{Resource, ResourceExt};
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

/// What every workspace of an owner carries about them, from the directory the api tier owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerMaterial {
    /// The `authorized_keys` file sshd reads. Empty is a user with no keys.
    pub authorized_keys: String,
    /// What git commits as. Empty when the handle is nobody's, and git will ask.
    pub git_name: String,
    pub git_email: String,
}

/// The three lookups this api makes against the platform directory, kept behind a trait rather
/// than a direct dependency on `rustic_git_pulls::directory::Directory` (mongo-backed, heavy to
/// construct) so unit tests can supply a stub instead. Production wires `Directory` in via an
/// adapter in `bins/api`.
///
/// Every method defaults to the UNWIRED answer, so a test stub implements only the lookups its
/// case exercises. That is NOT the same as a missing directory: `teams_for`'s default returns an
/// empty Vec, which `resolve_new_owner` reads as "asked and answered", so a partial stub gets a
/// 403 "not a member" where no directory at all gets a 503. Only test stubs are partial —
/// production wires the full `Dir` adapter in `bins/api`.
#[async_trait::async_trait]
pub trait Directory: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, _user: &str) -> Vec<String> {
        Vec::new()
    }

    /// Is this CLI login still valid? A `cli` JWT carries a `jti` whose row in the directory IS
    /// the revocation list — the same rule `crates/api`'s `user_identity` enforces. `false`
    /// refuses the token, which is what an unwired directory must do: a 30-day token nobody can
    /// cancel is the worse failure.
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }

    /// The owner's ssh keys and git identity. `None` when the lookup FAILED — distinct from `Some`
    /// with an empty `authorized_keys`, which is a user with no keys and is written as an empty
    /// file.
    async fn for_owner(&self, _owner: &str) -> Option<OwnerMaterial> {
        None
    }
}

pub struct ApiState {
    pub jwt: Arc<Jwt>,
    /// Emails allowed to hit the admin-gated region routes. See module docs.
    pub admins: HashSet<String>,
    /// Team membership, CLI-token revocation and the owner's ssh keys. `None` means no directory
    /// is wired (dev, or the directory tier is down): team envs answer 503 rather than silently
    /// behaving as if the caller has no teams, CLI tokens are refused outright, and a workspace
    /// comes up with the private key alone exactly as before ssh existed.
    pub directory: Option<Arc<dyn Directory>>,
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
    pub fn new(jwt: Arc<Jwt>, admins: HashSet<String>) -> Self {
        ApiState {
            jwt,
            admins,
            directory: None,
            kube: None,
            keys: None,
            upstream: None,
        }
    }

    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
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
    match &s.directory {
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
        .route("/v1/workspaces/{id}", get(get_ws).delete(delete_ws).patch(patch_ws_packages))
        .route("/v1/workspaces/{id}/clone", post(clone_ws))
        .route("/v1/workspaces/{id}/push", post(push_ws))
        .route("/v1/workspaces/{id}/start", post(start_ws))
        .route("/v1/workspaces/{id}/stop", post(stop_ws))
        .route("/v1/workspaces/{id}/attach", post(attach_ws))
        .route("/v1/workspaces/{id}/detach", post(detach_ws))
        .route("/v1/workspaces/{id}/ssh-session", post(ssh_session))
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

/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
async fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let tok = bearer_token(headers).ok_or_else(unauthorized)?;
    let (c, jti) = state.jwt.verify_any_user(tok.trim()).map_err(|_| unauthorized())?;
    // Only a CLI token carries a `jti`, and only a CLI token is revocable: a session's lifetime
    // IS its expiry. Without a directory to ask, a CLI token authenticates nothing here.
    // ponytail: one directory read per CLI request, no cache — same tradeoff `teams_for` takes;
    // add a short-TTL cache if it shows up hot, remembering it delays a revocation by its TTL.
    if let Some(jti) = jti {
        match &state.directory {
            Some(check) if check.is_live(&jti).await => {}
            _ => return Err(unauthorized()),
        }
    }
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

// ── regions ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    /// `active` or `inactive`. Re-registering a region is the only way to retire one — there is
    /// no delete — and a retired region must stop being offered to new workspaces while its
    /// existing records stay readable.
    #[serde(default = "active_status")]
    status: String,
}

fn active_status() -> String {
    "active".into()
}

/// What a caller sees: the three fields `check_region` and the web consume, and nothing about
/// where the region's infrastructure lives.
#[derive(serde::Serialize)]
struct RegionDoc {
    id: String,
    name: String,
    status: String,
}

fn region_doc(r: &crd::Region) -> RegionDoc {
    RegionDoc { id: r.name_any(), name: r.spec.name.clone(), status: r.spec.status.clone() }
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
    // The id becomes an object name and a gateway hostname label, so it goes through the same
    // segment check every other path segment here does.
    check_path_segment(&body.id)?;
    let status = if body.status == "inactive" { "inactive" } else { "active" };
    let r = crd::Region::new(&body.id, crd::RegionSpec { name: body.name, status: status.into() });
    // Apply, not create: re-registering IS how a region is retired or renamed, so a second POST of
    // the same id must not be a 409.
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let saved = api
        .patch(&body.id, &PatchParams::apply("rustic-git-api").force(), &Patch::Apply(&r))
        .await
        .map_err(kube_err)?;
    Ok((StatusCode::CREATED, Json(region_doc(&saved))).into_response())
}

async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers).await?;
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let rows: Vec<RegionDoc> =
        api.list(&ListParams::default()).await.map_err(kube_err)?.items.iter().map(region_doc).collect();
    Ok(Json(rows).into_response())
}

// ── the cluster ──────────────────────────────────────────────────────────

fn kube(s: &ApiState) -> Result<&kube::Client, Response> {
    s.kube.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "kubernetes not configured on this node").into_response()
    })
}

/// An API-server error keeps its own status where the caller can act on it (404 is "no such
/// workspace", 409 is "retry"); anything else is ours, not the caller's.
fn is_missing(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 404)
}

fn kube_err(e: kube::Error) -> Response {
    match &e {
        kube::Error::Api(ae) if ae.code == 404 => not_found(),
        kube::Error::Api(ae) if ae.code == 409 => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        _ => {
            tracing::error!(error = %e, "kubernetes");
            (StatusCode::INTERNAL_SERVER_ERROR, "kubernetes error").into_response()
        }
    }
}

pub use crate::k8s::{ATTACHED_ENV_LABEL, KIND_LABEL, OWNER_LABEL, TEAM_LABEL};

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

/// The `spec.owner` of anything this API lists. One trait so "narrow by label, DECIDE on spec" is
/// a single function instead of a rule seven handlers each remembered or forgot.
pub trait Owned {
    fn owner(&self) -> &str;
}

impl Owned for crd::Workspace {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}
impl Owned for crd::Environment {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}
impl Owned for crd::Snapshot {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}

/// Keep only what `owners` actually owns. The label selector stays as the INDEX; this is the
/// answer. An object whose label disagrees with its spec — a restored backup, a migration, an
/// operator with kubectl, the window before the controller re-stamps — is somebody else's.
pub fn mine<K: Owned>(items: Vec<K>, owners: &[String]) -> Vec<K> {
    items.into_iter().filter(|k| owners.iter().any(|o| o == k.owner())).collect()
}

/// A name is unique per (owner, team): it is also the directory the workspace mounts at inside
/// the person's shared home (`~/workspaces/<name>`), and two workspaces on one path would be two
/// workspaces one editor session cannot tell apart. The selector narrows the list; the decision
/// reads `spec` (labels are a view). ponytail: a Workspace written by another path without its
/// labels is invisible here until the controller re-stamps them — a window of one reconcile.
async fn refuse_taken_name(c: &kube::Client, owner: &str, team: &str, name: &str) -> Result<(), Response> {
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let list = api.list(&owned_in(owner, team)).await.map_err(kube_err)?;
    if list.items.iter().any(|w| w.spec.owner == owner && w.spec.team == team && w.spec.name == name) {
        return Err((StatusCode::CONFLICT, format!("a workspace named {name:?} already exists here")).into_response());
    }
    Ok(())
}

/// Refuse a create that would take this owner past their ceiling.
///
/// The two label-selected lists cost what `refuse_taken_name` already pays, and the DECISION reads
/// `spec.owner` (labels are a view): an object mislabelled onto someone else must not spend their
/// budget. Counted across both kinds — they share a node's memory and the pool.
async fn refuse_over_cap(_s: &ApiState, c: &kube::Client, owner: &str) -> Result<(), Response> {
    let max = crate::model::max_per_owner();
    let lp = owned_by(owner);
    let ws = Api::<crd::Workspace>::all(c.clone()).list(&lp).await.map_err(kube_err)?;
    let envs = Api::<crd::Environment>::all(c.clone()).list(&lp).await.map_err(kube_err)?;
    let mine = ws.items.iter().filter(|w| w.spec.owner == owner).count()
        + envs.items.iter().filter(|e| e.spec.owner == owner).count();
    if mine >= max {
        // 429, not 409: nothing conflicts with a particular object — this account is asking for
        // more than it may hold. The number is in the message so the person can ask for a raise.
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("you already have {mine} workspaces and environments; the limit is {max}"),
        )
            .into_response());
    }
    Ok(())
}

/// `status.phase` is the state, and an object the controller has not seen yet has no status at
/// all — `creating` rather than a `null` the web app's enum cannot parse.
fn phase<T: serde::de::DeserializeOwned>(p: Option<&str>, default: T) -> T {
    p.and_then(|p| serde_json::from_value(serde_json::json!(p)).ok()).unwrap_or(default)
}

/// The child `Volume`'s name, from STATUS alone: the reconciler creates the Volume and then
/// reports it, so that is the fact.
fn ws_volume(w: &crd::Workspace) -> Option<&str> {
    w.status.as_ref().and_then(|st| st.volume_ref.as_deref()).filter(|v| !v.is_empty())
}

fn env_volume(e: &crd::Environment) -> Option<&str> {
    e.status.as_ref().and_then(|st| st.volume_ref.as_deref()).filter(|v| !v.is_empty())
}

/// Every volume of `owner` that has ever landed a snapshot.
///
/// From the SERVER tier's volume index — the same listing the Snapshots page reads — because that
/// is the record: a push receipt is reclaimed by the owning node in time, and a listing that read
/// the receipts went blind on a volume a week after its last push. It is a QUERY rather than a Volume status field because a field would need a
/// second controller writing the Volume's status — `patch_status` force-applies under one field
/// manager, so the Volume reconciler's next pass would prune it.
///
/// ONE call per REQUEST, passed down to every row: one lookup per row turns a listing into an N+1.
/// With no server tier configured (a dev API with no git node) the label-selected receipts are
/// the fallback, for as long as they last.
async fn pushed_volumes(s: &ApiState, c: &kube::Client, owner: &str) -> Result<HashSet<String>, Response> {
    if let Some(up) = s.upstream.as_ref() {
        return Ok(up
            .volumes(owner, owner)
            .await
            .map_err(upstream_err)?
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.name)
            .collect());
    }
    // No server tier configured (a dev API with no git node): fall back to `Snapshot` CRs owned
    // by this label, deduplicated to their volume — the commit-model equivalent of the browse
    // tier's per-owner volume list.
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let items = mine(api.list(&owned_by(owner)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner.to_string()));
    Ok(items
        .into_iter()
        .filter(|s| s.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready))
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
        // Free-form live state was a job-era field the agent wrote back into the doc; the pod and
        // its status are the live state now. Kept in the body so the web app's parse is unchanged.
        live_state: serde_json::Value::Null,
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

/// The one gate on a workspace or environment name, on every route that accepts one. The name ends up verbatim
/// in generated ssh config on a TEAMMATE's machine (`model::valid_ws_name`), so it is checked
/// where it enters the system rather than at each renderer — the renderers refuse too, but a
/// stored bad name would already have made every listing of that team unusable.
fn check_ws_name(name: &str) -> Result<(), Response> {
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

/// A region is an id the caller typed, and it becomes the OwnerBinding's name and the gateway
/// hostname. Unknown: a workspace no controller ever claims. Chosen: a binding name squatted in
/// someone else's region. Only what an admin registered and left active gets through.
async fn check_region(s: &ApiState, region: &str) -> Result<(), Response> {
    check_path_segment(region)?;
    let api: Api<crd::Region> = Api::all(kube(s)?.clone());
    let active = api
        .get_opt(region)
        .await
        .map_err(kube_err)?
        .is_some_and(|r| r.spec.status == "active");
    if active {
        return Ok(());
    }
    Err((StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": "unknown region"}))).into_response())
}

/// `0` is a qgroup nothing can start on, and the upper end is more than the pool node can back.
/// Clamped rather than refused: the web sends a fixed default, and a client that asks for more
/// than the ceiling gets the ceiling.
/// ponytail: one global ceiling; make it per-region node capacity if a region ever has more.
fn clamp_quota(gb: u64) -> u64 {
    gb.clamp(1, 500)
}

async fn create_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewWorkspace>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    check_ws_name(&body.name)?;
    check_region(&s, &body.region).await?;
    let team = match body.team.as_deref().map(str::trim).filter(|t| !t.is_empty() && *t != owner) {
        None => String::new(),
        // 404, not 403: whether a team exists is not a non-member's to learn, same as every
        // other owner-scoped route.
        Some(t) if may_act_on(&s, &owner, t).await => t.to_lowercase(),
        Some(_) => return Err((StatusCode::NOT_FOUND, "no such team").into_response()),
    };
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;
    refuse_over_cap(&s, c, &owner).await?;
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
            storage: Some(crd::WorkspaceStorage { quota_gb: clamp_quota(body.quota_gb), source }),
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

/// Every namespace name the platform would derive for this owner: their personal one, plus one
/// per team they are in.
///
/// The label is a VIEW and never authority (CLAUDE.md) — the NAME is what says whose namespace
/// this is, so it is checked by RECOMPUTING it rather than by picking the owner back out of the
/// string. `crd::ws_namespace` hashes any name over 63 characters into a DNS label, which no
/// prefix/suffix test can invert: the earlier `ends_with("-{owner}")` heuristic skipped exactly
/// those, so an ssh key add never reached a workspace in a long-named team.
async fn owners_namespaces(s: &ApiState, owner: &str) -> HashSet<String> {
    let mut out = HashSet::from([crd::ws_namespace(owner, "")]);
    out.extend(teams_for(s, owner).await.iter().map(|t| crd::ws_namespace(owner, t)));
    out
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

async fn list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
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
    let items = mine(api.list(&owned_in(&owner, &team)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner));
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
async fn ssh_session(
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
                .filter(|w| w.spec.owner == owner)
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

async fn start_ws(
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
fn node_dead_warning(node_name: &str, conditions: &[crd::Condition]) -> Option<String> {
    interrupted(conditions)
        .then(|| format!("node {node_name} is down; edits after the last sync point are only on that node and will not follow the move"))
}

/// Interrupted: the node died while this was RUNNING, so its live edits exist only there. The
/// sweep writes `Degraded/NodeDead` and keeps the pin; nothing in the system may move it. Both the
/// type and the reason, not the reason alone — `NodeDead` is a specific enough token that nothing
/// else uses it today, but matching only half of what the sweep writes is how this and the sweep
/// drift apart the day something else reuses the reason on a different condition type.
fn interrupted(conditions: &[crd::Condition]) -> bool {
    conditions.iter().any(|c| c.type_ == "Degraded" && c.reason == "NodeDead" && c.status == "True")
}

/// The one answer a start gets while a parent is interrupted. There is deliberately no force
/// flag: abandoning someone's edits is not a thing this API can offer, and the way forward is a
/// clone from the last synced point — which `clone` allows, with its age stated.
fn interrupted_409(kind: &str) -> Response {
    (StatusCode::CONFLICT, format!("{kind} is interrupted: its node is down; it resumes when the node returns")).into_response()
}

async fn stop_ws(
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
    let mut doc = ws_doc(&w, &HashSet::new());
    doc.state = WsState::Stopped;
    let mut body = serde_json::to_value(&doc).expect("Workspace doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

#[derive(serde::Deserialize)]
struct AttachBody {
    environment: String,
}

/// Attach this workspace to an environment, so its services resolve by bare name.
///
/// A merge patch on the one field, for the same reason `set_desired` is one: this handler was sent
/// one field and must not claim ownership of a spec the caller never wrote. Spec only — every
/// visible effect of an attachment is the agent's reconcile.
async fn attach_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AttachBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
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
async fn detach_ws(
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
    let owner = caller(&s, &headers).await?;
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

/// What a clone was grafted onto. Always present on a clone response: a clone is always based on
/// a cut, and only the interrupted case makes that cut older than "now" — which is the one thing a
/// person needs to weigh before accepting it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BasedOn {
    pub snapshot: String,
    /// The cut's `readyAt` — absent for a cut this request just made, which is not Ready yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// How stale the copy is, in seconds, at the moment of this response. Zero for a fresh cut.
    pub age_seconds: i64,
    /// The source's node was down, so this is the newest cut a peer already HOLDS rather than a
    /// fresh one taken for this request.
    pub interrupted: bool,
}

/// Every `Snapshot` of `volume`, and the newest Ready transient of `worktree` among them as the
/// whole object: a clone's parent when the owner can cut, and the clone's own base when it cannot.
/// `crd::newest_transient_of` is the ordering, shared with the agent so `/v1` and placement can
/// never disagree about which cut is newest.
async fn newest_transient(c: &kube::Client, volume: &str, worktree: &str) -> Result<(Option<crd::Snapshot>, Vec<crd::Snapshot>), Response> {
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let list = api
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(kube_err)?;
    let newest = crd::newest_transient_of(&list.items, worktree);
    let found = newest.and_then(|n| list.items.iter().find(|s| s.name_any() == n).cloned());
    Ok((found, list.items))
}

/// Same one-cut-in-flight rule `create_snapshot` enforces: a second Working cut of one worktree
/// forks the transient chain, and the loser then misdescribes what it holds.
///
/// Only where a cut is about to be TAKEN. An interrupted source's Working cut belongs to the node
/// that died holding it: it will never converge and nothing will ever clear it, so refusing on it
/// would close the one door left open — cloning off a copy a peer already holds.
fn refuse_cut_in_flight(all: &[crd::Snapshot], worktree: &str) -> Result<(), Response> {
    if all.iter().any(|sn| sn.spec.worktree == worktree && sn.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Working)) {
        return Err((StatusCode::CONFLICT, "a snapshot is already being cut for this workspace").into_response());
    }
    Ok(())
}

/// Every transient of `worktree` that some node's `VolumeReplica` reports HOLDING — the candidate
/// set a clone of an interrupted source may graft onto, because its own node cannot serve a byte.
/// Read-only, and only here: `status.branches` is the pulling agent's to write, always.
async fn replicated_transients(c: &kube::Client, volume: &str, worktree: &str) -> Result<HashSet<String>, Response> {
    let api: Api<crd::VolumeReplica> = Api::all(c.clone());
    Ok(api
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter_map(|r| r.status.and_then(|st| st.branches.get(worktree).cloned()))
        .collect())
}

/// Seconds between `at` and now, floored at 0. An unparseable or absent timestamp is 0: the age is
/// advisory, and a clone must never fail because a clock string did not parse.
fn age_seconds(at: Option<&str>) -> i64 {
    at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0))
        .unwrap_or(0)
}

/// What this clone grafts onto, and — in the ordinary case — the act of creating it.
///
/// The cut is taken HERE rather than left to the next sync beat because a clone that leaned on the
/// last beat could be a whole `WS_SYNC_SECS` stale: silent data loss nobody asked for. It is a
/// `clone-{worktree}-{hex}` TRANSIENT, the same shape the sync beat produces, so the puller sends a
/// delta against what a replica already holds and retention sweeps it like any other sync point.
///
/// An INTERRUPTED source is the one exception: its node is down, so nothing can be cut there at
/// all. The clone then grafts onto the newest transient — which, by the up-to-date rule, is exactly
/// the one an up-to-date node holds — and the response states its age so the person chooses the
/// gap knowingly. With no transient anywhere there is nothing to graft onto and no way forward.
/// Returns the cut UNCREATED (`Some`) in the ordinary case: the caller creates the workspace first
/// and only then writes it. A create that fails after the cut leaves a `Working` Snapshot nothing
/// will ever fulfil, which then blocks the next clone on the one-cut-in-flight guard.
async fn clone_base(
    c: &kube::Client,
    owner: &str,
    volume: &str,
    worktree: &str,
    interrupted: bool,
    parent_ref: Option<OwnerReference>,
    state: crd::SnapshotState,
) -> Result<(BasedOn, Option<crd::Snapshot>), Response> {
    let (newest, all) = newest_transient(c, volume, worktree).await?;
    if interrupted {
        // Not the newest transient cluster-wide — the newest one another node actually HOLDS. The
        // owner is down, so a cut it turned Ready seconds before it died may exist nowhere else at
        // all, and grafting onto that leaves the clone unplaceable forever. `status.branches` on a
        // `VolumeReplica` is the only record of who holds what, and the up-to-date rule placement
        // applies reads exactly the same field.
        let held = replicated_transients(c, volume, worktree).await?;
        let newest_held = crd::newest_transient_of(&all.iter().filter(|s| held.contains(&s.name_any())).cloned().collect::<Vec<_>>(), worktree);
        let held = newest_held.and_then(|n| all.into_iter().find(|s| s.name_any() == n)).ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "the source's node is down and no other node holds a sync point of it yet; nothing can be cloned until one syncs or the node returns",
            )
                .into_response()
        })?;
        let at = held.status.as_ref().and_then(|st| st.ready_at.clone());
        return Ok((BasedOn { snapshot: held.name_any(), age_seconds: age_seconds(at.as_deref()), at, interrupted: true }, None));
    }
    refuse_cut_in_flight(&all, worktree)?;
    let name = format!("clone-{worktree}-{}", crd::short_hex());
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: volume.to_string(),
            owner: owner.to_string(),
            worktree: worktree.to_string(),
            // The previous sync point, so the puller sends a delta. Empty on a source that has
            // never been snapshotted at all, exactly as a root commit is.
            parent: newest.map(|s| s.name_any()).unwrap_or_default(),
            message: Some("cloning".to_string()),
            transient: true,
            state: Some(state),
        },
    );
    // `status` on CREATE is stored verbatim, which is how this is born `Working` and reaches the
    // owning node's snapshot reconciler rather than sitting at the schema's `Pending` default.
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    snap.metadata.labels = Some(crd::commit_labels(owner, volume));
    // Owned by the source parent, exactly as the sync beat's cuts are: deleting the source is the
    // whole delete, and a cut nothing points at would otherwise outlive it as a leaked subvolume.
    snap.metadata.owner_references = parent_ref.map(|r| vec![r]);
    Ok((BasedOn { snapshot: name, at: None, age_seconds: 0, interrupted: false }, Some(snap)))
}

/// Attach `based_on` to a doc the way `stop_ws` attaches `warning`: a key beside the doc's own
/// fields, so the web client's `res.json()` of a Workspace/Environment keeps working unchanged.
fn with_based_on<T: serde::Serialize>(doc: &T, based_on: &BasedOn) -> Response {
    let mut body = serde_json::to_value(doc).expect("doc always serializes");
    body["based_on"] = serde_json::to_value(based_on).expect("BasedOn always serializes");
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// The one local-copy route.
///
/// It names no node: placement is the claim's job now, and the ONE rule — a node up to date for the
/// SOURCE worktree — is read there. At the instant of the cut above the owner is simply the only
/// node that qualifies, so a running source's clone lands on the owner by arithmetic; there is no
/// "same node" rule here or anywhere.
async fn clone_ws(
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
    refuse_over_cap(&s, c, &owner).await?;
    let new_id = rid("ws");
    let volume = ws_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
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
            owner,
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
    Ok(with_based_on(&ws_doc(&w, &HashSet::new()), &based_on))
}

/// What a copy of `volume` should be sized at.
///
/// A release-1 object created before `spec.storage` existed carries no quota, and 0 is NOT a
/// "controller default" — it would size the btrfs qgroup straight to zero. The quota of a legacy
/// source lives on its Volume, which is the object the controller sizes the disk from, so read it
/// there rather than inventing a number.
const FALLBACK_QUOTA_GB: u64 = crd::DEFAULT_WS_QUOTA_GB;
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

/// The region the bytes actually live in, read off the detached `Volume` a restore grafts onto.
///
/// A restore's whole point is that the source workspace may be gone, and "default" is then a guess
/// that lands the new pod in a region whose nodes hold none of these snapshots.
async fn volume_region(c: &kube::Client, volume: &str) -> Option<String> {
    let vols: Api<crd::Volume> = Api::all(c.clone());
    // An unreadable Volume, or one written before regions existed (`region` is a plain String, so
    // "no region" is the empty one), leaves the caller's own fallback in charge.
    vols.get_opt(volume).await.ok().flatten().map(|v| v.spec.region).filter(|r| !r.is_empty())
}

/// A workspace whose `Volume` the controller has not reported yet: 409, not a 500 and not a
/// silently dropped request. The caller can retry in a second.
fn not_ready() -> Response {
    (StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response()
}

#[derive(serde::Deserialize)]
struct RestoreBody {
    name: String,
    // The `snapshot_id` alone is a Snapshot CR name (Task 8) — the old registry-scoped `volume`
    // hint that used to turn a multi-volume scan into one read no longer means anything, since
    // `find_commit_model_snapshot_for_restore` looks the CR up by name directly.
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
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    check_ws_name(&body.name)?;
    // Restore-to-new IS a clone at a named commit (Task 8): under the commit model there is no
    // registry to fetch from any more, so this resolves the request's `snapshot_id` — a `Snapshot`
    // CR name — straight against the CRD, and the new workspace's source becomes
    // `CloneOf{volume, commit: Some(id)}`, exactly `Engine::clone_local_ids`/`checkout`'s own
    // shared-worktree path (Task 6b). `find_commit_model_snapshot_for_restore` is the owner check:
    // CR exists, Ready, and the caller may read `spec.owner` — anything else is a 404, same as a
    // missing snapshot, so a caller learns nothing about volumes that are not theirs.
    let snap = find_commit_model_snapshot_for_restore(&s, &owner, &body.snapshot_id).await?;
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
    let src = my_ws(&s, &owner, &volume).await.ok();
    let team = src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default();
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;
    refuse_over_cap(&s, c, &owner).await?;

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
    let new_id = rid("ws");
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner,
            team: src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default(),
            name: body.name,
            // No per-snapshot region under the commit model (single-pool, replica-based; cross-
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
    crd::DEFAULT_ENV_QUOTA_GB
}

/// Resolve `NewEnvironment.owner` against the caller: personal (`None` or `caller`) always
/// passes; a different owner must be a team the caller belongs to, which needs a directory —
/// 503 rather than silently creating an environment nobody but this caller can ever see again.
async fn resolve_new_owner(s: &ApiState, caller: &str, owner: Option<String>) -> Result<String, Response> {
    let Some(owner) = owner else { return Ok(caller.to_string()) };
    if owner == caller {
        return Ok(owner);
    }
    match &s.directory {
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

async fn create_env(
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
    refuse_over_cap(&s, c, &owner).await?;
    let id = rid("env");
    let e = create_environment(
        c,
        &id,
        crd::EnvironmentSpec {
            owner,
            name: body.name,
            region: body.region,
            services: body.services,
            storage: Some(crd::WorkspaceStorage { quota_gb: clamp_quota(body.quota_gb), source: None }),
            desired_state: DesiredState::Running,
            restore: None,
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
/// `restoreOf` source for an Environment.
///
/// The services default to what the snapshot froze beside the bytes (`SnapshotState`), because an
/// environment's data without its services is not the environment. A non-empty body list overrides
/// it; an empty one means the same as none. An environment always has services.
async fn restore_env(
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
    // Restore-to-new is a clone at a named commit under the commit model (Task 8) — see
    // `restore_ws`'s matching comment. `find_commit_model_snapshot_for_restore` is the ownership
    // check: CR exists, Ready, and the caller may read `spec.owner`.
    let snap = find_commit_model_snapshot_for_restore(&s, &caller_id, &body.snapshot_id).await?;
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
    if body.owner.as_deref().is_some_and(|o| o != src_owner && o != caller_id) {
        return Err((StatusCode::FORBIDDEN, "a snapshot restores under its own owner, or under you").into_response());
    }
    let owner = resolve_new_owner(&s, &caller_id, body.owner.or(Some(src_owner.clone()))).await?;
    refuse_over_cap(&s, kube(&s)?, &owner).await?;
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
        (Some(q), _) => clamp_quota(q),
        (None, Some(f)) => clamp_quota(f.1),
        (None, None) => default_env_quota(),
    };
    let c = kube(&s)?;
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
            // No per-snapshot region under the commit model (see `restore_ws`'s matching comment).
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
    let caller_id = caller(&s, &headers).await?;
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
        let pushed = pushed_volumes(&s, c, &owner).await?;
        for e in mine(api.list(&owned_by(&owner)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner)) {
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

async fn start_env(
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
    Ok((StatusCode::ACCEPTED, Json(env_doc(&e, &HashSet::new()))).into_response())
}

async fn stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    set_desired::<crd::Environment>(kube(&s)?, &id, DesiredState::Stopped).await?;
    let mut doc = env_doc(&e, &HashSet::new());
    doc.state = EnvState::Stopped;
    let warning = e.status.as_ref().and_then(|st| node_dead_warning(&st.node_name, &st.conditions));
    let mut body = serde_json::to_value(&doc).expect("Environment doc always serializes");
    if let Some(w) = warning {
        body["warning"] = serde_json::Value::String(w);
    }
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

async fn delete_env(
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
    match wss.list(&attached_to).await {
        Ok(list) => {
            for w in list.items.iter().filter(|w| w.spec.attached_environment.as_deref() == Some(id.as_str())) {
                let patch = serde_json::json!({"spec": {"attachedEnvironment": serde_json::Value::Null}});
                if let Err(e) = wss.patch(&w.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await {
                    tracing::warn!(workspace = %w.name_any(), error = %e, "clearing an attachment");
                }
            }
        }
        Err(err) => tracing::warn!(environment = %id, error = %err, "listing workspaces to clear attachments; some may still name this environment"),
    }
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
    // `CommitPending` guard in this controller that `resolve_volume` would then need).
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

async fn push_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let msg = optional_push_message(body).await?;
    let volume = ws_volume(&w).ok_or_else(not_ready)?;
    let head = w.status.as_ref().and_then(|st| st.head.clone());
    let state = crd::SnapshotState::of_workspace(&w);
    create_snapshot(kube(&s)?, volume, &w.spec.owner, &id, head, msg, state).await
}

async fn push_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let msg = optional_push_message(body).await?;
    let volume = env_volume(&e).ok_or_else(not_ready)?;
    let head = e.status.as_ref().and_then(|st| st.head.clone());
    let state = crd::SnapshotState::of_environment(&e);
    create_snapshot(kube(&s)?, volume, &e.spec.owner, &id, head, msg, state).await
}

/// A `Snapshot` CR, created `Working` so the agent's `reconcile_commit` can act on the very first
/// pass — CR-first (module doc).
async fn create_snapshot(
    c: &kube::Client,
    volume: &str,
    owner: &str,
    worktree: &str,
    parent: Option<String>,
    message: Option<String>,
    state: crd::SnapshotState,
) -> Result<Response, Response> {
    // F1: two pushes of the same worktree before the first is cut both read the same `head` and
    // both claim it as `parent` — the loser becomes a Ready commit no worktree's `head` ever
    // reaches, and `worktree_heads`/retention only walks the WINNER's chain, so the loser is never
    // revisited: an unbounded CR+disk leak, with a `parent` that misdescribes what it holds. A
    // worktree may have at most one `Working` cut in flight at a time — refuse the second here,
    // before it exists, rather than reconcile two winners later.
    let api: Api<crd::Snapshot> = Api::all(c.clone());
    let racing = api
        .list(&ListParams::default().fields(&format!("spec.volume={volume}")))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .any(|sn| sn.spec.worktree == worktree && sn.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Working));
    // ponytail: list-then-create leaves a TOCTOU sliver — two pushes landing between each
    // other's list and create both slip through and fork the chain. The 409 closes the common
    // case (a user double-clicking, a retrying client); the sliver's cost is one orphan commit,
    // and the upgrade path is a deterministic Working-CR name per worktree so the second create
    // itself collides.
    if racing {
        return Err((StatusCode::CONFLICT, "a snapshot is already being cut for this workspace").into_response());
    }
    let name = crd::snapshot_name(volume);
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: volume.to_string(),
            owner: owner.to_string(),
            worktree: worktree.to_string(),
            parent: parent.unwrap_or_default(),
            message,
            // Not transient: a push IS a snapshot (`Snapshot::is_snapshot`). It is kept until
            // someone deletes it by hand (`delete_snapshot`), never pruned by retention, and it
            // keeps its Volume alive after the workspace is gone (`cleanup_parent`).
            transient: false,
            state: Some(state),
        },
    );
    snap.metadata.labels = Some(crd::commit_labels(owner, volume));
    // `status` on CREATE is stored verbatim (the subresource split only governs UPDATE/PATCH), so
    // this is how the object is born `Working` instead of the schema's `Pending` default —
    // `reconcile_commit` only ever acts on `Working`.
    // Owned by the Volume so the commit record goes with it: a commit CR with no owner outlived
    // its deleted workspace once, and its snapshot subvolume sat on a node with nothing left to
    // reap it. The agent's own cuts (sync points, stops) are owned the same way, via the parent.
    let vol = match Api::<crd::Volume>::all(c.clone()).get(volume).await {
        Ok(v) => v,
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            return Err((StatusCode::NOT_FOUND, "the volume this worktree is on no longer exists").into_response())
        }
        Err(e) => return Err(kube_err(e)),
    };
    snap.metadata.owner_references =
        Some(vec![vol.controller_owner_ref(&()).expect("a live Volume has a uid")]);
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    api.create(&PostParams::default(), &snap).await.map_err(kube_err)?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id": name, "phase": crd::Phase::Working.as_str()}))).into_response())
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
    let caller_id = caller(&s, &headers).await?;
    let e = find_env(&s, &caller_id, &id).await?;
    let volume = env_volume(&e).ok_or_else(not_ready)?.to_string();
    // The wish names a `Snapshot` CR of this environment's OWN volume — validated Ready and
    // same-volume BEFORE the wish is written, so a bad id is a fast 4xx here rather than a silent
    // hang in `restore_gate` (which reads the wish uncritically, per its own doc comment).
    let snap = find_commit_model_snapshot(&s, &caller_id, &volume, &body.snapshot_id).await?;
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
    /// Epoch millis of the volume's last write. Approximate by construction — the newest
    /// `Snapshot` CR's creation time, sync points included.
    latest_ms: Option<i64>,
    /// How many pushes are on this volume. Any phase but `Error`, matching `cleanup_parent`'s
    /// rule: a push still being cut is a snapshot the person is waiting for, not one to hide.
    snapshots: u64,
    /// `readyAt` of the newest push, RFC3339; `None` while the only push is still being cut.
    last_push_at: Option<String>,
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

/// `OWNER_LABEL in (…)`, built only from slugs that are single validated segments.
///
/// `in (a,b)` is comma-delimited and paren-terminated, so one slug carrying `,` or `)` widens or
/// breaks the set — on a listing that decides whether a row says "source deleted". Slugs are
/// directory-validated today; every other selector in this file takes a single validated value,
/// and this one now does too.
pub fn owner_set_selector(owners: &[String]) -> String {
    // `owners` is always `caller_owners`'s output, and that always starts with the caller's own
    // (already-validated) owner — so this never filters down to an empty set.
    let safe: Vec<&str> =
        owners.iter().filter(|o| rustic_git_storage::store::valid_owner(o)).map(String::as_str).collect();
    format!("{OWNER_LABEL} in ({})", safe.join(","))
}

/// A live Workspace/Environment, reduced to what the volume routes need of it.
struct Parent {
    kind: String,
    display: String,
    /// `status.head` — the snapshot it is standing on, which `delete_snapshot` refuses to remove.
    head: Option<String>,
    /// The snapshot its SPEC was grafted onto. `head` only exists once the owning node has checked
    /// out; between the create and that first checkout the spec is the only record that this
    /// snapshot is load-bearing, and deleting it there is unrecoverable.
    base: Option<String>,
}

/// The snapshot a parent's volume source names, if any.
fn source_snapshot(storage: &Option<crd::WorkspaceStorage>) -> Option<String> {
    match storage.as_ref()?.source.as_ref()? {
        VolumeSource::CloneOf { commit, .. } => commit.clone(),
        VolumeSource::SeededFrom { snapshot, .. } => Some(snapshot.clone()),
        _ => None,
    }
}

/// The live parents, by the volume they are ON, with the kind they are. One list call per kind,
/// never one per row.
///
/// `None` means the cluster could not be asked, which is NOT the same as "nothing is alive": the
/// difference decides whether every row on the page is labelled "source deleted" during a kube
/// blip. The caller keeps `deleted: false` on `None` — the snapshots are what the page is for, and
/// they are all still there. The delete routes treat `None` as "cannot prove nothing is running"
/// and refuse, which is the opposite bias and the right one there.
///
/// Keyed on `status.volumeRef` where there is one, the parent's own name otherwise: a restored
/// environment holds a SECOND worktree on the source's volume, and keying on the name alone made
/// it invisible to both the listing and the head check.
///
/// Both kinds are selected by the caller's whole owner set, not just their own label: a team's
/// workspace is one they may read, and a head check that could not see it would let a delete take
/// a running worktree's base out from under it.
async fn live_parents(s: &ApiState, owners: &[String]) -> Option<BTreeMap<String, Parent>> {
    let c = s.kube.as_ref()?;
    let lp = ListParams::default().labels(&owner_set_selector(owners));
    let mut live = BTreeMap::new();
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    for w in mine(ws.list(&lp).await.ok()?.items, owners) {
        let st = w.status.as_ref();
        let vol = st.and_then(|s| s.volume_ref.clone()).unwrap_or_else(|| w.name_any());
        live.insert(
            vol,
            Parent {
                kind: "workspace".into(),
                display: w.spec.name.clone(),
                head: st.and_then(|s| s.head.clone()),
                base: source_snapshot(&w.spec.storage),
            },
        );
    }
    let envs: Api<crd::Environment> = Api::all(c.clone());
    for e in mine(envs.list(&lp).await.ok()?.items, owners) {
        let st = e.status.as_ref();
        let vol = st.and_then(|s| s.volume_ref.clone()).unwrap_or_else(|| e.name_any());
        live.insert(
            vol,
            Parent {
                kind: "environment".into(),
                display: e.spec.name.clone(),
                head: st.and_then(|s| s.head.clone()),
                base: source_snapshot(&e.spec.storage),
            },
        );
    }
    Some(live)
}

/// Every live Workspace/Environment ON `volume`, whoever owns it — the check both deletes make
/// before they take anything away.
///
/// UNLABELLED, unlike `live_parents`: a shared clone or a restore-to-new puts a worktree owned by a
/// DIFFERENT owner on the same volume (`CloneOf`), and an owner-scoped listing cannot see it. That
/// blind spot let a delete take another owner's running base out from under their pod, so this
/// listing is cluster-wide and matches on `status.volumeRef` (the parent's own name for a parent
/// that has not recorded one yet), exactly as the agent's own retention does.
///
/// `None` means the cluster could not be asked — "cannot prove nothing is running", which both
/// callers turn into a refusal rather than a delete.
async fn parents_of_volume(s: &ApiState, volume: &str) -> Option<Vec<Parent>> {
    let c = s.kube.as_ref()?;
    // Placed parents come back by the indexed field. Unplaced ones — created seconds ago, or
    // waiting on a node that is down — have no `volumeRef` at all, and the API server indexes that
    // as the empty string: a small, bounded set whose `spec.storage.source` says what they graft
    // onto. Both, because Task 5's protection depends on the second.
    let placed = ListParams::default().fields(&format!("status.volumeRef={volume}"));
    let unplaced = ListParams::default().fields("status.volumeRef=");
    let mut out = vec![];
    for lp in [&placed, &unplaced] {
        for w in Api::<crd::Workspace>::all(c.clone()).list(lp).await.ok()?.items {
            let st = w.status.as_ref();
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), w.name_any(), &w.spec.storage) {
                out.push(Parent {
                    kind: "workspace".into(),
                    display: w.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base: source_snapshot(&w.spec.storage),
                });
            }
        }
        for e in Api::<crd::Environment>::all(c.clone()).list(lp).await.ok()?.items {
            let st = e.status.as_ref();
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), e.name_any(), &e.spec.storage) {
                out.push(Parent {
                    kind: "environment".into(),
                    display: e.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base: source_snapshot(&e.spec.storage),
                });
            }
        }
    }
    Some(out)
}

/// On this volume: the node said so, the parent IS the volume (an owned one shares its id), or its
/// spec grafts onto it and no node has answered yet.
fn on_volume(volume: &str, vref: Option<String>, name: String, storage: &Option<crd::WorkspaceStorage>) -> bool {
    vref.unwrap_or(name) == volume
        || matches!(
            storage.as_ref().and_then(|s| s.source.as_ref()),
            Some(VolumeSource::CloneOf { volume: v, .. } | VolumeSource::SeededFrom { volume: v, .. }) if v == volume
        )
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
    let caller_id = caller(&s, &headers).await?;
    let owners = match &q.owner {
        Some(o) if may_act_on(&s, &caller_id, o).await => vec![o.clone()],
        Some(_) => return Err(not_found()),
        None => caller_owners(&s, &caller_id).await,
    };

    // The rows ARE the snapshots: one label-selected list, grouped by `spec.volume`. The registry
    // volume index is not consulted at all any more — a push writes a `Snapshot` CR and nothing
    // else, so a listing that read the index would have gone blind on everything pushed since.
    let api: Api<crd::Snapshot> = Api::all(kube(&s)?.clone());
    let snaps = mine(api.list(&ListParams::default().labels(&owner_set_selector(&owners))).await.map_err(kube_err)?.items, &owners);

    // The cluster answers only "does a parent still exist", so this degrades the page rather than
    // emptying it. `None` is an unanswered question, never an answer of "nothing": labelling every
    // row "source deleted" during a blip is the failure mode this distinction exists to prevent.
    let live = live_parents(&s, &owners).await;
    let known = live.is_some();
    let live = live.unwrap_or_default();

    let mut by_volume: BTreeMap<String, Vec<&crd::Snapshot>> = BTreeMap::new();
    for sn in &snaps {
        by_volume.entry(sn.spec.volume.clone()).or_default().push(sn);
    }

    let mut keep: Vec<VolumeSummary> = vec![];
    for (name, rows) in by_volume {
        // Any phase but `Error`, and never a sync point — `cleanup_parent`'s rule, so what keeps a
        // volume alive there is exactly what this counts.
        let pushes: Vec<&&crd::Snapshot> = rows
            .iter()
            .filter(|sn| {
                sn.is_snapshot() && sn.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
            })
            .collect();
        // A volume whose only records are sync points has nothing to show or restore — it is a
        // live worktree's replication state, not history, and was never a row on this page.
        if pushes.is_empty() {
            continue;
        }
        let owner = rows.first().map(|sn| sn.spec.owner.clone()).unwrap_or_default();
        let parent = live.get(&name);
        let kind = parent
            .map(|p| p.kind.clone())
            // The frozen `spec.state` is tagged by kind, so a deleted parent still says what it
            // was without a second read; the id prefix is the last resort for a legacy record.
            .or_else(|| {
                pushes.first().and_then(|sn| sn.spec.state.as_ref()).map(|st| match st {
                    crd::SnapshotState::Environment { .. } => "environment".to_string(),
                    crd::SnapshotState::Workspace { .. } => "workspace".to_string(),
                })
            })
            .unwrap_or_else(|| kind_of(&name));
        keep.push(VolumeSummary {
            kind,
            display_name: parent.map(|p| p.display.clone()).unwrap_or_else(|| name.clone()),
            deleted: known && parent.is_none(),
            volume: Some(format!("vol/{owner}/{name}")),
            latest_ms: rows.iter().filter_map(|sn| sn.creation_timestamp()).map(|t| t.0.as_millisecond()).max(),
            snapshots: pushes.len() as u64,
            last_push_at: pushes.iter().filter_map(|sn| sn.status.as_ref()?.ready_at.clone()).max(),
            name,
        });
    }

    if let Some(kind) = &q.kind {
        keep.retain(|v| &v.kind == kind);
    }
    keep.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(keep).into_response())
}

/// A volume name or snapshot id from the URL is spliced into a PEER url by `Upstream`, so a
/// `..` or an encoded slash would re-route the request to any browse route under the caller's
/// own owner. The same rule the create path applies to the names it mints.
fn check_path_segment(s: &str) -> Result<(), Response> {
    match rustic_git_storage::store::valid_segment(s) {
        true => Ok(()),
        false => Err((StatusCode::BAD_REQUEST, "invalid name").into_response()),
    }
}

/// `DELETE /v1/volumes/{name}` — delete a volume and every snapshot on it. What the environment
/// Delete dialog calls when "Also delete its snapshots" is checked, and what an archived row's
/// "Delete snapshots" calls on its own.
///
/// A volume the caller may not read is a 404 rather than a 403 — they learn nothing about volumes
/// that are not theirs. A volume that still has a live parent is a 409: its bytes are somebody's
/// working copy, and deleting the Volume out from under a running worktree is not a snapshot
/// operation. Deleting the Volume CR takes every `Snapshot` on it (they are its children) and the
/// agent's byte sweep reclaims the subvolumes.
async fn delete_volume(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // The ownership check IS the snapshot listing: a volume with no `Snapshot` under a label the
    // caller may read is indistinguishable from one that does not exist.
    commit_model_snapshots(&s, &caller_id, &name).await?;
    // Deleting the Volume CR cascades to every Snapshot on it, so a volume carrying somebody
    // else's push is not this caller's to collect — the owner-filtered listing above cannot even
    // see those, which is how one team member's delete used to take the team's whole history.
    let owners: HashSet<String> = caller_owners(&s, &caller_id).await.into_iter().collect();
    if snapshots_on_volume(&s, &name).await?.iter().any(|sn| !owners.contains(&sn.spec.owner)) {
        return Err((
            StatusCode::CONFLICT,
            "this volume also holds snapshots owned by someone else; delete your own snapshots instead",
        )
            .into_response());
    }
    // A cluster that could not be listed is "cannot prove nothing is running" — the opposite bias
    // to the listing's, and the right one for a delete.
    if !parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?.is_empty() {
        return Err((StatusCode::CONFLICT, "the volume still has a workspace or environment").into_response());
    }
    delete_volume_cr(&s, &name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn kube_unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "the cluster could not be reached").into_response()
}

/// 404 on an already-gone object: two clicks on the same Delete button is a race, not an error.
async fn delete_volume_cr(s: &ApiState, name: &str) -> Result<(), Response> {
    let api: Api<crd::Volume> = Api::all(kube(s)?.clone());
    match api.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
        Err(e) => Err(kube_err(e)),
    }
}

/// `DELETE /v1/volumes/{name}/snapshots/{snapshot}` — delete ONE snapshot.
///
/// 404 for the two cases that must stay indistinguishable: a volume the caller may not read, and
/// an id that is not on it. 409 for the two that are refusals rather than absences: a sync point
/// (the agent owns those — deleting one by hand deletes a replica's send parent), and a snapshot a
/// running worktree is standing on.
///
/// Deleting the LAST snapshot of a volume nothing owns any more deletes the volume too: that is
/// what Task 1-3 detached it for, and leaving it behind would leak a subvolume nothing can ever
/// reach again.
async fn delete_snapshot(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((name, snapshot)): Path<(String, String)>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    check_path_segment(&snapshot)?;
    let items = commit_model_snapshots(&s, &caller_id, &name).await?;
    let target = items.iter().find(|sn| sn.name_any() == snapshot).ok_or_else(not_found)?;
    // `is_snapshot`, not `spec.transient`: a legacy migration baseline is a sync point by shape
    // rather than by flag, and deleting one by hand still removes a replica's send parent.
    if !target.is_snapshot() {
        return Err((StatusCode::CONFLICT, "a sync point cannot be deleted by hand").into_response());
    }
    let live = parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?;
    // EVERY parent on the volume, not just the caller's: a shared clone's worktree belongs to
    // another owner, and its head is just as much a running base as this owner's own.
    if live.iter().any(|p| p.head.as_deref() == Some(snapshot.as_str()) || p.base.as_deref() == Some(snapshot.as_str())) {
        return Err((StatusCode::CONFLICT, "this snapshot is the base of a running worktree").into_response());
    }
    let api: Api<crd::Snapshot> = Api::all(kube(&s)?.clone());
    match api.delete(&snapshot, &Default::default()).await {
        Ok(_) => {}
        // Already gone: someone got there first, which is the outcome the caller asked for.
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(kube_err(e)),
    }
    // The same rule `cleanup_parent` detached the Volume under (Task 2d), read from the other end:
    // it survives its parent only for as long as a snapshot needs it. Both halves are RE-READ
    // here rather than reused from above: a restore or a clone can attach a working copy, and
    // another push can land, in the window between those reads and this delete — deciding on the
    // stale view would delete a volume somebody just started using.
    let items = snapshots_on_volume(&s, &name).await?;
    let live = parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?;
    let remaining = items.iter().any(|sn| {
        sn.name_any() != snapshot
            && sn.is_snapshot()
            && sn.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
    });
    if !remaining && live.is_empty() {
        delete_volume_cr(&s, &name).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The commit-model reads for `/history` and `/refs`: `Snapshot` CRs instead of registry
/// records. Scoped by `caller_owners` exactly like `volume_owner` — a volume under a label the
/// caller may not read is a 404, same as the registry path.
async fn commit_model_snapshots(s: &ApiState, caller_id: &str, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    let items = commit_model_snapshots_maybe_empty(s, caller_id, name).await?;
    if items.is_empty() {
        return Err(not_found());
    }
    Ok(items)
}

/// `commit_model_snapshots`, minus the "no rows" 404 — F6: `/refs` on a workspace that has never
/// pushed has zero `Snapshot` CRs, which is a real, ownable volume with no commits yet, not an
/// unknown one; the registry path answers that with `{"main": null}`, never 404, and this is what
/// lets `volume_refs` match it.
async fn commit_model_snapshots_maybe_empty(s: &ApiState, caller_id: &str, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    check_path_segment(name)?;
    let owners: HashSet<String> = caller_owners(s, caller_id).await.into_iter().collect();
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    let list = api
        .list(&ListParams::default().fields(&format!("spec.volume={name}")))
        .await
        .map_err(kube_err)?;
    let mut items: Vec<crd::Snapshot> = list.items.into_iter().filter(|sn| owners.contains(&sn.spec.owner)).collect();
    // F2: NEWEST first, matching the registry path's order (`records.first()` is always its
    // tip) — a consumer rendering history the old way would show it backwards otherwise.
    items.sort_by_key(|sn| std::cmp::Reverse(sn.creation_timestamp().map(|t| t.0)));
    Ok(items)
}

/// Every snapshot on `volume`, whoever owns it — the same bias `parents_of_volume` takes, and for
/// the same reason: a restore or a shared clone puts another owner's snapshots on this volume, and
/// a decision that DESTROYS data must see them. The owner-filtered listings above stay what the
/// caller may read; this one is only ever counted, never returned.
async fn snapshots_on_volume(s: &ApiState, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    check_path_segment(name)?;
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    Ok(api
        .list(&ListParams::default().fields(&format!("spec.volume={name}")))
        .await
        .map_err(kube_err)?
        .items)
}

/// A single `Ready` commit-model snapshot of `volume`, scoped by `caller_owners` exactly like
/// `commit_model_snapshots`. Used by restore: a 404 here is "unknown", "not yours", "not this
/// volume's", or "not cut yet" alike — the caller only needs to know it cannot restore onto it,
/// never which of those it was, the same way `find_snapshot`'s registry twin already collapses
/// "no such snapshot" and "not yours" into one 404.
async fn find_commit_model_snapshot(
    s: &ApiState,
    caller_id: &str,
    volume: &str,
    snapshot_id: &str,
) -> Result<crd::Snapshot, Response> {
    check_path_segment(snapshot_id)?;
    let owners: HashSet<String> = caller_owners(s, caller_id).await.into_iter().collect();
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    let snap = api.get_opt(snapshot_id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    let ready = snap.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready);
    if snap.spec.volume != volume || !owners.contains(&snap.spec.owner) || !ready {
        return Err(not_found());
    }
    Ok(snap)
}

/// Same ownership/readiness check as `find_commit_model_snapshot`, for a caller that does not yet
/// know the volume — restore-to-new (`restore_ws`/`restore_env`), where the snapshot id is all the
/// client has and `spec.volume` is exactly what this resolves.
async fn find_commit_model_snapshot_for_restore(s: &ApiState, caller_id: &str, snapshot_id: &str) -> Result<crd::Snapshot, Response> {
    check_path_segment(snapshot_id)?;
    let owners: HashSet<String> = caller_owners(s, caller_id).await.into_iter().collect();
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    let snap = api.get_opt(snapshot_id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    let ready = snap.status.as_ref().is_some_and(|st| st.phase == crd::Phase::Ready);
    if !owners.contains(&snap.spec.owner) || !ready {
        return Err(not_found());
    }
    Ok(snap)
}

/// Every row is a SNAPSHOT. A sync point is the agent's replication state, not something the
/// person took, and the migration baseline is not either — showing them as history offers a
/// restore onto a record that can vanish on the next sync beat.
fn snapshot_rows(items: &[crd::Snapshot]) -> Vec<serde_json::Value> {
    items
        .iter()
        .filter(|sn| sn.is_snapshot())
        .map(|sn| {
            let phase = sn.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Pending);
            serde_json::json!({
                "id": sn.name_any(),
                "state": serde_json::to_value(&sn.spec.state).unwrap_or(serde_json::Value::Null),
                "lineage": [],
                "region": "",
                "message": sn.spec.message,
                // F3: `jiff::Timestamp`'s `Display` IS RFC3339 (`2026-01-01T00:00:00Z`), the
                // same shape `chrono::DateTime<Utc>`'s serde impl gives the registry path's
                // `created_at` — asserted directly in `history_created_at_is_rfc3339`
                // rather than trusted, since a jiff/chrono formatting drift would be silent otherwise.
                "createdAt": sn.creation_timestamp().map(|t| t.0.to_string()),
                "parent": if sn.spec.parent.is_empty() { serde_json::Value::Null } else { serde_json::json!(sn.spec.parent) },
                "phase": phase.as_str(),
            })
        })
        .collect()
}

async fn volume_history(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    let items = commit_model_snapshots(&s, &caller_id, &name).await?;
    Ok(Json(snapshot_rows(&items)).into_response())
}

/// There is exactly one ref per volume ("main") and its value is always the newest snapshot — the
/// same "first = tip" convention `engine::ops` relies on.
async fn volume_refs(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let caller_id = caller(&s, &headers).await?;
    // F6: never 404 here — a zero-commit volume is `{"main": null}`.
    let items = commit_model_snapshots_maybe_empty(&s, &caller_id, &name).await?;
    // Never a sync point: `main` is what a clone or a restore grafts onto, and retention deletes
    // every sync point but the newest.
    let tip = items.iter().find(|sn| sn.is_snapshot()).map(|sn| sn.name_any());
    Ok(Json(serde_json::json!({"main": tip})).into_response())
}

#[cfg(test)]
mod tests {
    /// Kube error text names endpoints, keys and query shapes; the caller gets a fixed body and
    /// the log gets the detail.
    #[tokio::test]
    async fn backend_error_text_never_reaches_the_caller() {
        let body = |r: axum::response::Response| async move {
            String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 4096).await.unwrap()).into_owned()
        };
        let e = kube::Error::Api(Box::new(kube::core::Status::failure("AccountEndpoint=https://secret", "InternalError").with_code(500)));
        let r = super::kube_err(e);
        assert_eq!(r.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body(r).await.contains("secret"));
    }

    /// `delete_ws` must not stop at a 404 from the Workspace delete — that's the race the
    /// reorder was meant to cover, another caller already deleted it — and still has to fall
    /// through to collect the environment-side policy.
    #[test]
    fn a_404_from_the_workspace_delete_is_not_an_error() {
        let missing = kube::Error::Api(Box::new(kube::core::Status::failure("workspaces.rustic-git.io \"ws-1\" not found", "NotFound").with_code(404)));
        assert!(super::is_missing(&missing));
        let other = kube::Error::Api(Box::new(kube::core::Status::failure("conflict", "Conflict").with_code(409)));
        assert!(!super::is_missing(&other));
    }

    use super::{check_services, ws_doc};
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
                resources: Default::default(),
                packages: vec![],
                attached_environment: None,
            },
        )
    }

    /// A team namespace is `wt-{owner}-{hash}` (and a long personal one is DNS-hashed), so it is
    /// exactly the case the old `ends_with("-{owner}")` heuristic dropped — and dropping it meant
    /// an ssh key add never reached that team's workspaces.
    #[tokio::test]
    async fn a_dns_truncated_team_namespace_is_still_the_owners() {
        use super::{owners_namespaces, ApiState, Directory};
        use std::sync::Arc;

        let long = "a".repeat(60);
        struct Stub(String);
        #[async_trait::async_trait]
        impl Directory for Stub {
            async fn teams_for(&self, _user: &str) -> Vec<String> {
                vec![self.0.clone()]
            }
        }
        let state = ApiState::new(
            Arc::new(rustic_git_core::jwt::Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap()),
            Default::default(),
        )
        .with_directory(Arc::new(Stub(long.clone())));

        let ns = crd::ws_namespace("karthik", &long);
        assert!(ns.len() <= 63 && !ns.ends_with("-karthik"), "this team must be hashed: {ns}");
        let mine = owners_namespaces(&state, "karthik").await;
        assert!(mine.contains(&ns), "{ns} must be recognised as karthik's");
        assert!(mine.contains(&crd::ws_namespace("karthik", "")));
        assert!(!mine.contains(&crd::ws_namespace("someone-else", "")));
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
