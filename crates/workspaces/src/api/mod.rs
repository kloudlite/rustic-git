//! User-facing `/v1` routes for workspaces, environments and regions — spec §API
//! "User-facing (existing bearer token auth)".
//!
//! Every mutation writes a CUSTOM RESOURCE and answers 202 with a projection of it. The object is
//! the work item: there is no queue, no lease and no dispatch — the node named by `spec.nodeName`
//! reconciles what it owns. `Region` is a CRD too (`crd::Region`) — cross-cluster metadata by
//! nature, but registered rarely enough that the cluster this tier already talks to is the
//! cheapest correct home for it.
//!
//! Auth mirrors `crates/api`'s `caller()`: a Bearer JWT identifies the owner. Nothing superadmin-
//! only lives on this router: region creation, quota decisions and every cross-owner surface are
//! in `admin` (`/admin/*`, its own process under `KLOUDLITE_API_ROLE=admin`), which refuses a
//! token without the `superadmin` claim before routing. Here the claim is read only by
//! `may_act_on`'s third arm, and only for list/stop/delete/get — every ALLOCATING path (create,
//! clone, restore, push) decides its new object's owner through `scope::may_allocate_for`
//! instead, which never reads it: a superadmin is a claim, never an owner, and must not be able
//! to spend a team's quota without being a member. The static email allowlist this used to carry
//! is gone; `KLOUDLITE_WORKSPACES_ADMINS` is a bootstrap for the directory's list and nothing
//! reads it here.
//!
//! Split across `scope` (who the caller is, what they may act on), `workspaces`, `environments`,
//! `volumes` and `push` (I7) — one module per resource, this file keeps only what is shared by
//! all of them: `ApiState`, the router, auth, and the small set of error/lookup helpers every
//! handler in every submodule calls.

// Same idiom and same tradeoff as `crates/api`: `Result<T, Response>` is the handler style here,
// and boxing the Err to please the size lint would add an allocation per refusal for nothing.
#![allow(clippy::result_large_err)]

use crate::crd;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use kube::ResourceExt;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kloudlite_core::httpx::bearer_token;
use kloudlite_core::jwt::Jwt;
use kloudlite_core::settings::LiveSettings;
use crate::settings::AgentSettings;
use std::sync::Arc;

pub mod admin;
mod environments;
mod push;
pub(crate) mod scope;
mod volumes;
// `pub`: the SLO probe reads `KNOWN_CENTRAL` so its rollout yield asks about exactly the
// workloads a roll moves — one list, not a second copy that drifts.
pub mod workloads;
mod workspaces;

// The crate's public surface is unchanged by the split: `bins/api` and the tests name
// `api::{router, ApiState, Directory, …}` and must keep doing so. `admin` is `pub` (unlike its
// siblings) because its handlers are reached from `bins/api/src/main.rs` choosing which router to
// mount, not only through `router()` here.
pub use scope::{owner_set_selector, Owned};
pub use workspaces::refresh_user_keys;

use environments::{
    clone_env, create_env, delete_env, get_env, list_env, restore_env,
    restore_env_in_place, start_env, stop_env,
};
use push::{push_env, push_ws};
use volumes::{delete_snapshot, delete_volume, list_volumes, volume_history, volume_refs};
use workspaces::{
    attach_ws, clone_ws, create_ws, delete_ws, detach_ws, get_ws, list_ws, patch_ws_packages,
    restore_ws, ssh_session, start_ws, stop_ws,
};

/// Who is calling, and whether they hold the platform-administrator claim.
///
/// A struct rather than the bare handle because two facts travel together everywhere: the owner
/// name every path is scoped by, and the claim `may_act_on` reads as its third arm. `Deref` and
/// `Display` are so the sites that only want the handle read unchanged.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    /// A CLAIM from the session token, minted at sign-in from the directory's list. Never an
    /// ownership: it decides who may act, never who owns anything, and it never widens a quota.
    pub superadmin: bool,
}

impl std::ops::Deref for Caller {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// What every workspace of an owner carries about them, from the directory the api tier owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerMaterial {
    /// The `authorized_keys` file sshd reads. Empty is a user with no keys.
    pub authorized_keys: String,
    /// What git commits as. Empty when the handle is nobody's, and git will ask.
    pub git_name: String,
    pub git_email: String,
}

/// A person's standing in a team, as the platform directory records it.
///
/// A local enum rather than `kloudlite_pulls::directory::Role` for the same reason the whole
/// `Directory` trait is local: this crate must not depend on the mongo-backed one just for a
/// lookup. `Ord` is declared by the variant ORDER — `Member < Admin < Owner` — so `>= Admin` is
/// the rank rule, and there is no second rank table to fall out of step with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamRole {
    Member,
    Admin,
    Owner,
}

/// The four lookups this api makes against the platform directory, kept behind a trait rather
/// than a direct dependency on `kloudlite_pulls::directory::Directory` (mongo-backed, heavy to
/// construct) so unit tests can supply a stub instead. Production wires `Directory` in via an
/// adapter in `bins/api`.
///
/// Every method is REQUIRED. A defaulted one made a partial test stub read as a live-but-empty
/// directory — `teams_for` returning an empty Vec is "asked and answered" to `resolve_new_owner`,
/// which is a 403 "not a member", where no directory at all is a 503. A stub must say which it
/// means.
#[async_trait::async_trait]
pub trait Directory: Send + Sync {
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;

    /// Is this CLI login still valid? A `cli` JWT carries a `jti` whose row in the directory IS
    /// the revocation list — the same rule `crates/api`'s `user_identity` enforces. `false`
    /// refuses the token, which is what an unwired directory must do: a 30-day token nobody can
    /// cancel is the worse failure.
    async fn is_live(&self, jti: &str) -> bool;

    /// The owner's ssh keys and git identity. `None` when the lookup FAILED — distinct from `Some`
    /// with an empty `authorized_keys`, which is a user with no keys and is written as an empty
    /// file.
    async fn for_owner(&self, owner: &str) -> Option<OwnerMaterial>;

    /// The caller's role in `team`, or `None` when they are not a member — or when the lookup
    /// could not be made. Both answer "no" here, which is the safe direction for the one decision
    /// it feeds: who may raise a team's ceiling.
    ///
    /// `user` is whatever identity `teams_for` matches on, so the two can never disagree about who
    /// is in the team. Required (no default): unlike the other lookups, a stub that silently
    /// answered "not a member" would make the admin-only request check a no-op nobody tests.
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole>;

    /// Does a team named `slug` exist? The one thing a slug's spelling cannot say by itself, and
    /// the quota routes need it to pick `default_quota(team)`'s right side rather than guess from
    /// who happens to be asking. Required (no default): a stub answering `false` here would make
    /// every team-owned quota request silently seed from the person defaults, and nothing would
    /// fail loudly enough to notice.
    async fn is_team(&self, slug: &str) -> bool;

    /// Put `user` into `team` at `role`, creating the membership if they are not in it yet. Only
    /// the admin process implements this — the user role has no route that could call it.
    ///
    /// `user` is the HANDLE the request was opened under, the same identity `team_role` takes; an
    /// implementation whose store keys memberships on something else (the directory keys on email)
    /// resolves it itself, and answers `NoSuchUser` when it cannot.
    /// Create-or-find a person by email and give them `username` (a no-op when they already hold
    /// it). Only the SLO probe's bootstrap calls this, through `/admin/slo/bootstrap`: sign-in is the
    /// one other way a person comes to exist, and a synthetic user never signs in. `Err` carries
    /// the directory's own sentence.
    async fn ensure_user(&self, email: &str, name: &str, username: &str) -> Result<(), String>;
    /// Seat `email` on the superadmin roster, idempotently. Same single caller as `ensure_user`:
    /// the roster routes read the caller's ROW, so a probe that grants and revokes needs one.
    async fn add_superadmin(&self, email: &str, by: &str) -> Result<(), String>;

    async fn grant_access(&self, _team: &str, _user: &str, _role: TeamRole) -> GrantAccess {
        GrantAccess::Unsupported
    }
}

/// What a membership write did. Not a `Result`: "no such user" and "no such team" are answers a
/// decider needs to read back verbatim, not errors to log.
#[derive(Debug, PartialEq, Eq)]
pub enum GrantAccess {
    Done,
    NoSuchUser,
    NoSuchTeam,
    /// The directory's own refusal, already in words fit to show — a last-owner demotion, say.
    Refused(String),
    /// No directory is wired, or this one cannot write. A DEFAULT so every test stub in this crate
    /// keeps compiling; the approve arm turns it into a 503 rather than a false success.
    Unsupported,
}

pub struct ApiState {
    pub jwt: Arc<Jwt>,
    /// Team membership, CLI-token revocation and the owner's ssh keys. `None` means no directory
    /// is wired (dev, or the directory tier is down): team envs answer 503 rather than silently
    /// behaving as if the caller has no teams, CLI tokens are refused outright, and a workspace
    /// comes up with the private key alone exactly as before ssh existed.
    pub directory: Option<Arc<dyn Directory>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace, environment and
    /// volume route answers 503 rather than not existing.
    pub kube: Option<kube::Client>,
    /// The AKS in-cluster client — `kloudlite-admin`'s OWN cluster, distinct from `kube` above
    /// (a region's k3s, reached over a mounted kubeconfig). Only `admin::workloads`' central-scope
    /// calls use this; every CRD (workspaces, environments, regions, quotas) still lives in a
    /// region cluster and keeps reading `kube`. `None` off-AKS (dev, tests): central rolls answer
    /// 503 rather than not existing, same convention as `kube`.
    pub aks: Option<kube::Client>,
    /// The auth store: workspace creation copies the owner's platform-issued git key into their
    /// namespace through it, and `GET /admin/settings/central` reads `cluster/settings` off its
    /// `.os` object-store handle directly (this tier can read the object store anywhere, matching
    /// `_catalog`/`/api/{owner}/images` — only the write is peer-only). `None` in dev and in
    /// tests: workspaces still create without a key, and the central settings route answers 503.
    pub keys: Option<Arc<kloudlite_storage::store::Store>>,
    /// This tier's own region's `default_replicas`/`quota_gb_ceiling` — Task 3 gives the agent
    /// its own handle from a `ClusterSettings` reflector; this one seeds from env only, since
    /// `/v1` has no per-region watch of its own yet. Read-mostly today (no refresh beat wired
    /// here in this task), but the field exists so `clamp_quota` has a live ceiling to read
    /// instead of a compiled-in number.
    pub settings: LiveSettings<AgentSettings>,
    /// The server tier's peer listener + peer secret — the ONE call this admin process makes
    /// outbound to the git tier, forwarding a validated central-settings write (`PUT
    /// /api/admin/settings`, Task 4). `None` in dev/tests: `GET /admin/settings/central` still
    /// answers from `keys`' object store directly, only the `PUT` needs this.
    pub peer: Option<admin::PeerClient>,
    /// ClickHouse (ClickStack's), holding the collector's `default` telemetry and our own `kloudlite`
    /// database. `None` when `KLOUDLITE_CLICKHOUSE_URL` is unset — a supported configuration, not
    /// a degraded one: history routes answer `503 history unavailable` and the console renders a
    /// flat placeholder. Only the ADMIN process ever sets this; the user role never constructs one,
    /// which is what makes "the admin process is the only writer of `kloudlite`" a fact about the
    /// binary rather than a convention.
    pub history: Option<Arc<crate::history::History>>,
    /// Redis, for the `history` consumer group only — no request path reads it. `None` in dev and
    /// wherever the cache is disabled; the consumer then never spawns, which costs the activity
    /// feed its PR half and nothing else (CLAUDE.md: the stream is a nudge, never the record).
    pub cache: Option<Arc<kloudlite_storage::cache::Cache>>,
    /// `KLOUDLITE_SLO_WEBHOOK`, read once at boot. `None` — the default — means a failed probe
    /// run and a firing `SloBurn` are recorded and shown on the console like every other fact, and
    /// nothing is posted anywhere: the webhook is a nudge, never the record.
    pub slo_webhook: Option<String>,
}

impl ApiState {
    pub fn new(jwt: Arc<Jwt>) -> Self {
        ApiState {
            jwt,
            directory: None,
            kube: None,
            aks: None,
            keys: None,
            settings: LiveSettings::new(AgentSettings::from_env()),
            peer: None,
            history: None,
            cache: None,
            slo_webhook: None,
        }
    }

    pub fn with_peer(mut self, peer: admin::PeerClient) -> Self {
        self.peer = Some(peer);
        self
    }

    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
        self
    }

    pub fn with_kube(mut self, client: kube::Client) -> Self {
        self.kube = Some(client);
        self
    }

    pub fn with_aks(mut self, client: kube::Client) -> Self {
        self.aks = Some(client);
        self
    }

    pub fn with_keys(mut self, keys: Arc<kloudlite_storage::store::Store>) -> Self {
        self.keys = Some(keys);
        self
    }

    pub fn with_history(mut self, history: Arc<crate::history::History>) -> Self {
        self.history = Some(history);
        self
    }

    pub fn with_cache(mut self, cache: Arc<kloudlite_storage::cache::Cache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// An empty value is no webhook: an env var set to "" in a manifest must not become a post to
    /// the empty url on every failed run.
    pub fn with_slo_webhook(mut self, url: Option<String>) -> Self {
        self.slo_webhook = url.filter(|u| !u.trim().is_empty());
        self
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/quota", get(get_quota))
        .route("/v1/quota-requests", post(create_quota_request).get(list_quota_requests))
        .route("/v1/requests", post(create_request).get(list_requests))
        .route("/v1/requests/{id}", get(get_request))
        .route("/v1/regions", get(list_regions))
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

// ── quota ───────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct QuotaQuery {
    /// Absent means the caller's own. A team slug they belong to is allowed; anything else is a
    /// 404, same as every other owner-scoped read.
    #[serde(default)]
    owner: Option<String>,
}

/// `GET /v1/quota?owner=` — the ceiling and what is against it, both for one owner.
///
/// Usage is computed here and nowhere else, on every request (see `quota::usage`'s module doc).
async fn get_quota(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<QuotaQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let owner = q.owner.unwrap_or_else(|| c.name.clone());
    if !scope::may_act_on(&s, &c, &owner).await {
        return Err(not_found());
    }
    let client = kube(&s)?;
    let team = scope::is_team(&s, &owner).await;
    let limit = crate::quota::effective(client, &owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
    Ok(Json(serde_json::json!({"owner": owner, "limit": limit, "used": used})).into_response())
}

#[derive(serde::Deserialize)]
struct NewQuotaRequest {
    /// Absent means the caller's own quota.
    #[serde(default)]
    owner: Option<String>,
    requested: crd::RequestedQuota,
    #[serde(default)]
    reason: String,
}

/// Who may ask, and for whom.
///
/// A person may always ask for their own. A team's ceiling is a team decision, so only a member
/// whose directory role is at least admin may ask on its behalf — checked against the DIRECTORY,
/// never against a label and never against who happens to have created something.
async fn may_request_for(s: &ApiState, caller: &str, owner: &str) -> Result<(), Response> {
    if owner == caller {
        return Ok(());
    }
    let Some(dir) = &s.directory else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response());
    };
    match dir.team_role(caller, owner).await {
        Some(r) if r >= TeamRole::Admin => Ok(()),
        // A member gets the reason; a non-member learns nothing about the team at all.
        Some(_) => Err((StatusCode::FORBIDDEN, "only a team admin can request a team quota").into_response()),
        None => Err(not_found()),
    }
}

/// Every request of `owner`, label-selected — and re-checked against `spec.owner`, because the
/// label is a view.
async fn requests_of(c: &kube::Client, owner: &str) -> Result<Vec<crd::QuotaRequest>, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(c.clone());
    Ok(api
        .list(&scope::owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}

/// A request with no status yet is PENDING: `/v1` writes the object and stamps status in a second
/// call, and reading that window as "decided" would let two requests stand at once.
fn is_pending(r: &crd::QuotaRequest) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}

/// The pre-`Request` route, kept because the web's 409 dialog and `kl` both post here. It writes a
/// kind-quota `Request` now: one queue, one pending rule, one decision path — the old CRD is only
/// ever READ from here on.
async fn create_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewQuotaRequest>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let spec = crd::RequestSpec {
        owner: body.owner.unwrap_or_else(|| c.name.clone()),
        kind: crd::RequestKind::Quota,
        requested_by: c.name.clone(),
        reason: body.reason,
        quota: Some(body.requested),
        access: None,
        region: None,
        other: None,
    };
    let made = create_request_inner(&s, &c, spec).await?;
    Ok((StatusCode::CREATED, Json(generic_doc(&made))).into_response())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaRequestDoc {
    id: String,
    owner: String,
    requested: crd::RequestedQuota,
    reason: String,
    state: crd::RequestState,
    decided_by: Option<String>,
    decided_at: Option<String>,
    note: Option<String>,
    created_at: Option<String>,
}

fn request_doc(r: &crd::QuotaRequest) -> QuotaRequestDoc {
    let st = r.status.clone().unwrap_or_default();
    QuotaRequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        requested: r.spec.requested.clone(),
        reason: r.spec.reason.clone(),
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RequestQuery {
    #[serde(default)]
    owner: Option<String>,
}

/// The caller's own requests and their teams'. `owner` narrows to one, and must be something the
/// caller may act on — same rule as every other owner-scoped read.
async fn list_quota_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RequestQuery>,
) -> Result<Response, Response> {
    let caller = caller(&s, &headers).await?;
    let client = kube(&s)?;
    let mut rows = Vec::new();
    match q.owner {
        Some(owner) => {
            if !scope::may_act_on(&s, &caller, &owner).await {
                return Err(not_found());
            }
            rows.extend(requests_of(client, &owner).await?);
        }
        None => {
            for owner in scope::caller_owners(&s, &caller).await {
                rows.extend(requests_of(client, &owner).await?);
            }
        }
    }
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(request_doc).collect::<Vec<_>>()).into_response())
}

// ── generic requests ───────────────────────────────────────────────────

/// The generic queue's own doc. One shape for all four kinds — the block that is `None` is simply
/// absent, so a console renders "the facts for this kind" by reading the one field that is set.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestDoc {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) kind: crd::RequestKind,
    pub(crate) requested_by: String,
    pub(crate) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) quota: Option<crd::RequestedQuota>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) access: Option<crd::AccessAsk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) region: Option<crd::RegionAsk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) other: Option<crd::OtherAsk>,
    pub(crate) state: crd::RequestState,
    pub(crate) decided_by: Option<String>,
    pub(crate) decided_at: Option<String>,
    pub(crate) note: Option<String>,
    pub(crate) resolution: Option<String>,
    pub(crate) created_at: Option<String>,
}

pub(crate) fn generic_doc(r: &crd::Request) -> RequestDoc {
    let st = r.status.clone().unwrap_or_default();
    RequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        kind: r.spec.kind,
        requested_by: r.spec.requested_by.clone(),
        reason: r.spec.reason.clone(),
        quota: r.spec.quota.clone(),
        access: r.spec.access.clone(),
        region: r.spec.region.clone(),
        other: r.spec.other.clone(),
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        resolution: st.resolution,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

/// Every request of `owner`, label-selected — and re-checked against `spec.owner`, because the
/// label is a view.
pub(crate) async fn requests_of_generic(c: &kube::Client, owner: &str) -> Result<Vec<crd::Request>, Response> {
    let api: Api<crd::Request> = Api::all(c.clone());
    Ok(api
        .list(&scope::owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}

/// No status yet is PENDING — `/v1` writes the object and stamps status in a second call, and
/// reading that window as "decided" would let two requests of one kind stand at once.
pub(crate) fn is_pending_generic(r: &crd::Request) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}

#[derive(serde::Deserialize)]
struct NewRequest {
    /// Absent means the caller's own.
    #[serde(default)]
    owner: Option<String>,
    kind: crd::RequestKind,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    quota: Option<crd::RequestedQuota>,
    #[serde(default)]
    access: Option<crd::AccessAsk>,
    #[serde(default)]
    region: Option<crd::RegionAsk>,
    #[serde(default)]
    other: Option<crd::OtherAsk>,
}

/// The one place a `Request` is authored. Shared with the `/v1/quota-requests` wrapper so the
/// per-kind pending rule, the label and the author cannot be spelled twice.
pub(crate) async fn create_request_inner(
    s: &ApiState,
    caller: &Caller,
    spec: crd::RequestSpec,
) -> Result<crd::Request, Response> {
    spec.validate().map_err(|m| (StatusCode::UNPROCESSABLE_ENTITY, m).into_response())?;
    may_request_for(s, &caller.name, &spec.owner).await?;
    // A region has to be one an admin registered and left active — approving a grant for a region
    // that does not exist would record a decision nothing can ever honour.
    if let Some(r) = &spec.region {
        check_region(s, &r.region).await?;
    }
    let client = kube(s)?;
    // One at a time PER KIND, so each queue is a list of decisions rather than a list of the
    // same ask — and a pending access request never blocks an unrelated quota one.
    if requests_of_generic(client, &spec.owner)
        .await?
        .iter()
        .any(|r| is_pending_generic(r) && r.spec.kind == spec.kind)
    {
        return Err((StatusCode::CONFLICT, "a request is already pending").into_response());
    }
    let owner = spec.owner.clone();
    let mut r = crd::Request::new(&rid("req"), spec);
    // A view of `spec.owner`, so the queue and the owner's own list are indexed selectors — same
    // rule as every other label in this codebase.
    r.metadata.labels = Some(std::collections::BTreeMap::from([(OWNER_LABEL.to_string(), owner)]));
    let api: Api<crd::Request> = Api::all(client.clone());
    let out = api.create(&kube::api::PostParams::default(), &r).await.map_err(kube_err)?;
    // After the create, never before: a counted request that the API server refused is a queue
    // depth nobody can find. This is the one place a `Request` is authored (see the doc above).
    metrics::counter!("requests_opened_total", "kind" => out.spec.kind.as_str()).increment(1);
    Ok(out)
}

async fn create_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRequest>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let spec = crd::RequestSpec {
        owner: body.owner.unwrap_or_else(|| c.name.clone()),
        kind: body.kind,
        // From the claims, never the body: an author a request could name for itself is not
        // evidence of who asked.
        requested_by: c.name.clone(),
        reason: body.reason,
        quota: body.quota,
        access: body.access,
        region: body.region,
        other: body.other,
    };
    let made = create_request_inner(&s, &c, spec).await?;
    Ok((StatusCode::CREATED, Json(generic_doc(&made))).into_response())
}

/// The caller's own requests and their teams'. `owner` narrows to one, and must be something the
/// caller may act on — same rule as every other owner-scoped read.
async fn list_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RequestQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let client = kube(&s)?;
    let mut rows = Vec::new();
    match q.owner {
        Some(owner) => {
            if !scope::may_act_on(&s, &c, &owner).await {
                return Err(not_found());
            }
            rows.extend(requests_of_generic(client, &owner).await?);
        }
        None => {
            for owner in scope::caller_owners(&s, &c).await {
                rows.extend(requests_of_generic(client, &owner).await?);
            }
        }
    }
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(generic_doc).collect::<Vec<_>>()).into_response())
}

async fn get_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    check_path_segment(&id)?;
    let api: Api<crd::Request> = Api::all(kube(&s)?.clone());
    let r = api.get_opt(&id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    // 404, never 403: a refusal that distinguishes "not yours" from "no such id" confirms the id.
    if !scope::may_act_on(&s, &c, &r.spec.owner).await {
        return Err(not_found());
    }
    Ok(Json(generic_doc(&r)).into_response())
}

#[derive(serde::Deserialize, Default)]
struct Decision {
    #[serde(default)]
    note: Option<String>,
    /// The operator's edited ask, replacing `r.spec.requested` before `overlay` runs — approve
    /// grants what was actually submitted, which is the original request unless edited.
    #[serde(default)]
    requested: Option<crd::RequestedQuota>,
}

/// The ONE place `/v1` refuses an allocation.
///
/// Every route that brings a new working copy, a new disk or a new snapshot into existence goes
/// through here, so the sentence, the status and the read-then-write window are decided once (see
/// `quota::check`'s doc for why read-then-write is accepted rather than locked).
///
/// `owner` is the OBJECT's owner, never the caller: a team's working copies count against the
/// team and nobody else. A superadmin gets no exemption — the claim says who may act, never how
/// much may exist.
pub(crate) async fn guard_alloc(
    s: &ApiState,
    owner: &str,
    team: bool,
    want: &[(crate::quota::Dim, u64)],
) -> Result<(), Response> {
    let c = kube(s)?;
    let limit = crate::quota::effective(c, owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(c, owner).await.map_err(kube_err)?;
    for (dim, adding) in want {
        if let Err(msg) = crate::quota::check(*dim, &limit, &used, *adding) {
            // The single gate every create/restore/clone/push passes through, so one counter here
            // covers every refusal without a second one per handler. `dim.word()` is the same word
            // the 409 sentence uses, so a spike and the message a user saw name the same thing.
            metrics::counter!("quota_refusals_total", "dimension" => dim.word()).increment(1);
            return Err((StatusCode::CONFLICT, msg).into_response());
        }
    }
    Ok(())
}

// The design doc also lists "changing a volume's quota" and "changing resources". Neither has a
// route today (`/v1` has no resize and no resources patch — `patch_ws_packages` is packages only),
// so there is nothing to gate. A future resize route calls `guard_alloc` with the DELTA, never a
// check of its own: the sentence and the read-then-write window are decided here.

/// What a new workspace costs, from the values the handler has already resolved and clamped.
pub(crate) fn workspace_cost(quota_gb: u64, res: &crd::PodResources) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    vec![
        (Dim::Workspaces, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, millicores(&res.cpu_limit).div_ceil(1000)),
        (Dim::MemoryGb, mebibytes(&res.memory_limit).div_ceil(1024)),
    ]
}

/// The same for an environment: every service gets the env unit, one definition in `k8s`.
pub(crate) fn environment_cost(quota_gb: u64, services: usize) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    let unit = crate::k8s::env_unit_resources();
    let n = services as u64;
    vec![
        (Dim::Environments, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, (n * millicores(&unit.cpu_limit)).div_ceil(1000)),
        (Dim::MemoryGb, (n * mebibytes(&unit.memory_limit)).div_ceil(1024)),
    ]
}

pub(crate) fn rid(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}-{}", kloudlite_core::hex(&b))
}

/// The owner identity for everything workspace/environment/volume-shaped is the USERNAME,
/// not the email: volume paths (`vol/{owner}/{name}`) go through the same owner-name
/// validation as git repos, and an email's `@`/`.` can never route there. A token without a
/// chosen username cannot own workspaces yet — same rule the web app enforces for repos.
pub(crate) async fn caller(state: &ApiState, headers: &axum::http::HeaderMap) -> Result<Caller, Response> {
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
    let superadmin = c.superadmin;
    let name = c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })?;
    Ok(Caller { name, superadmin })
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
}

// ── regions ──────────────────────────────────────────────────────────────
//
// The write half (`create_region`) lives in `api::admin` now — a region is a platform decision.
// `list_regions` stays here: reading which regions exist is not superadmin-gated.

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

pub(crate) fn kube(s: &ApiState) -> Result<&kube::Client, Response> {
    s.kube.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "kubernetes not configured on this node").into_response()
    })
}

pub(crate) fn aks(s: &ApiState) -> Result<&kube::Client, Response> {
    s.aks.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "this node's own cluster is not configured").into_response()
    })
}

/// An API-server error keeps its own status where the caller can act on it (404 is "no such
/// workspace", 409 is "retry"); anything else is ours, not the caller's.
pub(crate) fn is_missing(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 404)
}

pub(crate) fn kube_err(e: kube::Error) -> Response {
    match &e {
        kube::Error::Api(ae) if ae.code == 404 => not_found(),
        kube::Error::Api(ae) if ae.code == 409 => (StatusCode::CONFLICT, "conflict, retry").into_response(),
        _ => {
            tracing::error!(reason = "kubernetes", error = %e, "request.failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "kubernetes error").into_response()
        }
    }
}

pub use crate::k8s::{ATTACHED_ENV_LABEL, KIND_LABEL, OWNER_LABEL, TEAM_LABEL};

/// `status.phase` is the state, and an object the controller has not seen yet has no status at
/// all — `creating` rather than a `null` the web app's enum cannot parse.
pub(crate) fn phase<T: serde::de::DeserializeOwned>(p: Option<&str>, default: T) -> T {
    p.and_then(|p| serde_json::from_value(serde_json::json!(p)).ok()).unwrap_or(default)
}

/// A region is an id the caller typed, and it becomes the OwnerBinding's name and the gateway
/// hostname. Unknown: a workspace no controller ever claims. Chosen: a binding name squatted in
/// someone else's region. Only what an admin registered and left active gets through.
pub(crate) async fn check_region(s: &ApiState, region: &str) -> Result<(), Response> {
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

/// The single `kube::Client` this tier holds today, but only after proving `region` names an
/// EXISTING active `crd::Region` — `admin::workloads`'s `Scope::Region(seg)` and the settings
/// routes both take `seg`/`region` straight off a URL path segment, so without this check a typo
/// or a probe would resolve to `kube(s)` anyway (there is only one client wired) and PATCH the
/// real cluster under a name nothing registered. `client_for` upgrades to a real per-region map
/// the day one exists; this is the one place both callers go through so that upgrade is a single
/// change (review finding on Task 5).
pub(crate) async fn client_for_region<'a>(s: &'a ApiState, region: &str) -> Result<&'a kube::Client, Response> {
    let client = kube(s)?;
    let api: Api<crd::Region> = Api::all(client.clone());
    match api.get_opt(region).await.map_err(kube_err)? {
        Some(r) if r.spec.status == "active" => Ok(client),
        _ => Err((StatusCode::NOT_FOUND, "no such region").into_response()),
    }
}

pub(crate) fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// A workspace whose `Volume` the controller has not reported yet: 409, not a 500 and not a
/// silently dropped request. The caller can retry in a second.
pub(crate) fn not_ready() -> Response {
    (StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response()
}

/// A volume name or snapshot id from the URL is spliced into a PEER url by `Upstream`, so a
/// `..` or an encoded slash would re-route the request to any browse route under the caller's
/// own owner. The same rule the create path applies to the names it mints.
pub(crate) fn check_path_segment(s: &str) -> Result<(), Response> {
    match kloudlite_storage::store::valid_segment(s) {
        true => Ok(()),
        false => Err((StatusCode::BAD_REQUEST, "invalid name").into_response()),
    }
}

pub(crate) fn kube_unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "the cluster could not be reached").into_response()
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
        let missing = kube::Error::Api(Box::new(kube::core::Status::failure("workspaces.kloudlite.io \"ws-1\" not found", "NotFound").with_code(404)));
        assert!(super::is_missing(&missing));
        let other = kube::Error::Api(Box::new(kube::core::Status::failure("conflict", "Conflict").with_code(409)));
        assert!(!super::is_missing(&other));
    }

    /// A directory that has not implemented granting answers `Unsupported`, and the approve arm
    /// turns that into a refusal — never a silent success on a membership nothing wrote.
    #[tokio::test]
    async fn a_directory_without_granting_refuses_rather_than_pretending() {
        use super::Directory as _;
        struct Bare;
        #[async_trait::async_trait]
        impl super::Directory for Bare {
            async fn teams_for(&self, _u: &str) -> Vec<String> {
                Vec::new()
            }
            async fn is_live(&self, _j: &str) -> bool {
                false
            }
            async fn for_owner(&self, _o: &str) -> Option<super::OwnerMaterial> {
                None
            }
            async fn team_role(&self, _u: &str, _t: &str) -> Option<super::TeamRole> {
                None
            }
            async fn is_team(&self, _s: &str) -> bool {
                false
            }
            async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
                Err("no directory".into())
            }
            async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
                Err("no directory".into())
            }
        }
        assert_eq!(
            Bare.grant_access("acme", "meera", super::TeamRole::Admin).await,
            super::GrantAccess::Unsupported
        );
    }
}
