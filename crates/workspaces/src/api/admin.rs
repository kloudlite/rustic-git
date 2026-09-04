//! Everything that needs the `superadmin` claim, on its OWN router — never mounted alongside
//! `api::router`. A `/v1` authorization bug cannot reach a handler here because the handler is not
//! in that binary's router at all; that separation is the whole reason this module exists rather
//! than a `require_admin` check inside `api::router` (design doc §5).
//!
//! Every handler below re-derives the owner from the request (a query param, a path segment, the
//! object being acted on) rather than from the caller — the caller here is never the owner, they
//! are the person acting ON an owner, and `may_act_on`'s claim arm (or, for workspaces/environments,
//! `my_ws`/`find_env`'s superadmin arm) is what makes that legitimate.

use super::*;
use axum::extract::Path;

mod audit;
mod clusters;
pub mod monitoring;
mod owners;
mod overview;
mod schema;
mod settings;

/// One row per successful write, called from the handler's own success path (see the module
/// doc on `crate::audit` for why this isn't a middleware). Fire-and-forget: audit is evidence,
/// not a gate, so a `put` failure is logged and swallowed rather than turned into a 5xx for a
/// write that already landed. `s.keys` unset (dev without an object store configured) means
/// there is nowhere to log to and nothing to block either, so this is a silent no-op then too —
/// every deployed admin process has `RUSTIC_GIT_S3_URL` wired (`deploy/rustic-git.yaml`).
pub(crate) async fn audit(
    s: &ApiState,
    actor: &str,
    action: &str,
    target: &str,
    reason: Option<String>,
    result: impl Into<std::borrow::Cow<'static, str>>,
) {
    let result = result.into();
    let Some(store) = s.keys.as_ref() else {
        tracing::warn!(actor, action, target, "audit row not written: no object store configured");
        return;
    };
    let entry = crate::audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        actor: actor.to_string(),
        action: action.to_string(),
        target: target.to_string(),
        reason,
        result,
    };
    if let Err(e) = crate::audit::record(&store.os, &entry).await {
        tracing::error!(error = %e, actor, action, target, "audit row not written");
    }
}

/// A refusal is evidence too: "we tried to drain that node and the API server said no" is exactly
/// what an operator reads the log for. Only 409 and up — a 4xx below that is a malformed request
/// that never reached a decision, and logging those would bury the real ones.
pub(crate) async fn audited<T>(
    s: &ApiState,
    actor: &str,
    action: &str,
    target: &str,
    reason: Option<String>,
    r: Result<T, Response>,
) -> Result<T, Response> {
    match r {
        Ok(v) => Ok(v),
        Err(resp) => {
            let code = resp.status().as_u16();
            if code >= 409 {
                audit(s, actor, action, target, reason, format!("error:{code}")).await;
            }
            Err(resp)
        }
    }
}

/// The Global Constraint's "reason on every write except approve", as one body shape: every
/// mutating admin route that has no more specific body takes this and nothing else.
#[derive(serde::Deserialize)]
pub(crate) struct NoteBody {
    #[serde(default)]
    pub(crate) note: String,
}

/// 422 on an empty or whitespace-only note, named the same way every other required field is.
pub(crate) fn require_note(note: &str) -> Result<String, Response> {
    let note = note.trim().to_string();
    if note.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "note is required").into_response());
    }
    Ok(note)
}

/// The one place a history route turns "no ClickStack" into a response. 503 with this exact
/// sentence, which the web keys its flat-placeholder rendering off — a bare status code would be
/// indistinguishable from a real outage.
// ponytail: no /admin/history route calls this yet (a later task adds them); unused-but-wired
// ahead of its callers, same as every other gate in this file was before its routes landed.
#[allow(dead_code)]
pub(crate) fn history_or_503(s: &ApiState) -> Result<&crate::history::History, Response> {
    s.history
        .as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "history unavailable").into_response())
}

pub use settings::PeerClient;

/// Runs before ANY handler on this router. A token that fails to verify is 401; one that verifies
/// but carries no claim is 403 — both before the request reaches a handler, which is what makes
/// `every_admin_path_refuses_without_the_claim`'s "zero calls" assertion true by construction
/// rather than by every handler remembering to check.
pub async fn refuse_without_claim(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(tok) = bearer_token(&headers) else {
        return unauthorized();
    };
    match s.jwt.verify_any_user(tok.trim()) {
        Ok((c, _)) if c.superadmin => next.run(req).await,
        Ok(_) => (StatusCode::FORBIDDEN, "admin only").into_response(),
        Err(_) => unauthorized(),
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/admin/regions", post(create_region))
        .route("/admin/quota/{owner}", axum::routing::put(write_quota_route))
        .route("/admin/overview", get(overview::overview_handler))
        .route("/admin/requests", get(list_requests))
        .route("/admin/requests/migrate", post(migrate_requests))
        .route("/admin/requests/{id}/approve", post(approve_request))
        .route("/admin/requests/{id}/deny", post(deny_request))
        .route("/admin/quota-requests", get(list_all_quota_requests))
        .route("/admin/quota-requests/{id}/approve", post(approve_quota_request))
        .route("/admin/quota-requests/{id}/deny", post(deny_quota_request))
        .route("/admin/owners", get(owners::owners_list))
        .route("/admin/owners/{slug}", get(owners::owner_detail))
        .route("/admin/nodes", get(list_nodes))
        .route("/admin/clusters", get(clusters::list_clusters))
        .route("/admin/clusters/{region}", get(clusters::cluster_detail))
        .route("/admin/clusters/{region}/status", axum::routing::put(clusters::set_region_status))
        .route("/admin/clusters/{region}/nodes/{node}/drain", post(clusters::drain))
        .route("/admin/clusters/{region}/nodes/{node}/undrain", post(clusters::undrain))
        .route("/admin/clusters/{region}/nodes/{node}/decommission", post(clusters::decommission))
        .route("/admin/workspaces", get(admin_list_ws))
        .route("/admin/workspaces/{id}", axum::routing::delete(admin_delete_ws))
        .route("/admin/workspaces/{id}/stop", post(admin_stop_ws))
        .route("/admin/environments", get(super::environments::list_env))
        .route("/admin/environments/{id}", axum::routing::delete(admin_delete_env))
        .route("/admin/environments/{id}/stop", post(admin_stop_env))
        .route("/admin/workloads", get(list_workloads_route))
        .route("/admin/workloads/{scope}/{name}/roll", post(roll_workload_route))
        .route("/admin/settings/central", get(settings::get_central).put(settings::put_central))
        .route("/admin/settings/central/revert", post(settings::revert_central))
        .route(
            "/admin/settings/clusters/{region}",
            get(settings::get_cluster).put(settings::put_cluster),
        )
        .route("/admin/settings/clusters/{region}/revert/{n}", post(settings::revert_cluster))
        .route("/admin/settings/schema", get(schema::get_schema))
        .route("/admin/monitoring/signals", get(monitoring::signals))
        .route("/admin/audit", get(audit::list_audit))
        .route("/admin/audit.csv", get(audit::audit_csv))
        // The claim check runs BEFORE every route above, not per-handler: `route_layer` wraps only
        // the routes already added, so a route added after this line would run unguarded — there
        // are none, and `every_admin_path_refuses_without_the_claim` is the tripwire if one is
        // ever added below it by mistake.
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), refuse_without_claim))
        .with_state(state)
}

// ── regions (moved from api::mod; body unchanged) ───────────────────────────

#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    /// `active` or `inactive`. Re-registering a region is the only way to retire one — there is
    /// no delete — and a retired region must stop being offered to new workspaces while its
    /// existing records stay readable.
    #[serde(default = "active_status")]
    status: String,
    /// Registering a region is a write like any other, so it carries its own reason rather than
    /// leaving the audit row's `why` column empty (Global Constraint).
    #[serde(default)]
    note: String,
}

fn active_status() -> String {
    "active".into()
}

async fn create_region(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRegion>,
) -> Result<Response, Response> {
    // The claim already gated this request in `refuse_without_claim`; no second check here — the
    // one place that decides is the layer every route on this router shares.
    let c = caller(&s, &headers).await?;
    check_path_segment(&body.id)?;
    let note = require_note(&body.note)?;
    let status = if body.status == "inactive" { "inactive" } else { "active" };
    let r = crd::Region::new(&body.id, crd::RegionSpec { name: body.name, status: status.into() });
    // Apply, not create: re-registering IS how a region is retired or renamed, so a second POST of
    // the same id must not be a 409.
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let action = if status == "inactive" { "deactivate-region" } else { "add-region" };
    let saved = audited(
        &s,
        &c.name,
        action,
        &body.id,
        Some(note.clone()),
        api.patch(&body.id, &PatchParams::apply("rustic-git-api").force(), &Patch::Apply(&r)).await.map_err(kube_err),
    )
    .await?;
    audit(&s, &c.name, action, &body.id, Some(note), "ok").await;
    Ok((StatusCode::CREATED, Json(region_doc(&saved))).into_response())
}

// ── quota decisions (moved from api::mod, Task 5b) ──────────────────────────

/// The one writer of a `Quota` object — `approve_quota_request` and `PUT /admin/quota/{owner}`
/// both call this, so the two paths that can set a limit can never disagree about how it lands.
async fn write_quota(s: &ApiState, owner: &str, spec: crd::QuotaSpec) -> Result<crd::Quota, Response> {
    let api: Api<crd::Quota> = Api::all(kube(s)?.clone());
    match api.get_opt(owner).await.map_err(kube_err)? {
        Some(_) => api
            .patch(owner, &PatchParams::default(), &Patch::Merge(&serde_json::json!({"spec": spec})))
            .await
            .map_err(kube_err),
        None => api.create(&PostParams::default(), &crd::Quota::new(owner, spec)).await.map_err(kube_err),
    }
}

#[derive(serde::Deserialize)]
struct WriteQuotaBody {
    spec: crd::QuotaSpec,
    /// Global Constraint: "set quota" is one of the writes that must carry a reason on the audit
    /// row, so it is required here rather than left `Option` like a roll's free-text reason.
    note: String,
}

async fn write_quota_route(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(owner): Path<String>,
    Json(body): Json<WriteQuotaBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = body.note.trim().to_string();
    if note.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "note is required").into_response());
    }
    let q = audited(&s, &c.name, "set-quota", &owner, Some(note.clone()), write_quota(&s, &owner, body.spec).await).await?;
    audit(&s, &c.name, "set-quota", &owner, Some(note), "ok").await;
    Ok(Json(q.spec).into_response())
}

async fn pending_request(s: &ApiState, id: &str) -> Result<crd::QuotaRequest, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(s)?.clone());
    let r = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if !is_pending(&r) {
        return Err((StatusCode::CONFLICT, "that request has already been decided").into_response());
    }
    Ok(r)
}

/// Overlay a request onto a limit. Only the dimensions the request NAMED move; approving must not
/// silently reset a limit somebody has already granted on another axis.
fn overlay(base: crd::QuotaSpec, want: &crd::RequestedQuota) -> crd::QuotaSpec {
    crd::QuotaSpec {
        workspaces: want.workspaces.unwrap_or(base.workspaces),
        environments: want.environments.unwrap_or(base.environments),
        snapshots: want.snapshots.unwrap_or(base.snapshots),
        disk_gb: want.disk_gb.unwrap_or(base.disk_gb),
        cpu: want.cpu.unwrap_or(base.cpu),
        memory_gb: want.memory_gb.unwrap_or(base.memory_gb),
        regions: base.regions,
    }
}

/// Stamp the outcome. `status`, not spec: the request is what was asked, the decision is what
/// happened to it, and only this tier ever writes it (no controller reconciles a request).
async fn decide(s: &ApiState, id: &str, state: crd::RequestState, by: &str, note: Option<String>) -> Result<Response, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(s)?.clone());
    let patch = serde_json::json!({"status": {
        "state": state, "decidedBy": by, "decidedAt": chrono::Utc::now().to_rfc3339(), "note": note,
    }});
    let out = api.patch_status(id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(Json(request_doc(&out)).into_response())
}

/// Approve: write the `Quota` FIRST, then mark the request — see `api::mod`'s doc on
/// `approve_quota_request` (unchanged reasoning, only the router moved).
async fn approve_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    // The DECIDING caller's name is still read here (for `decidedBy` and the base-quota guess
    // below), even though the claim itself was already checked by the layer.
    let c = caller(&s, &headers).await?;
    approve_legacy(&s, &c.name, &id, &legacy_body(&body)?).await
}

/// The legacy body, parsed. `/admin/quota-requests/…` and the generic route's fallback for an
/// un-migrated id decide the same object the same way, so they share one body rather than each
/// growing its own copy of the read-decide-mark order.
fn legacy_body(body: &axum::body::Bytes) -> Result<Decision, Response> {
    if body.is_empty() {
        return Ok(Decision::default());
    }
    serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())
}

async fn approve_legacy(s: &ApiState, actor: &str, id: &str, note: &Decision) -> Result<Response, Response> {
    // An already-decided request is a 409 the operator needs to see in the log — they raced
    // another admin — so the refusal is recorded against the request id, the only target known
    // before the read succeeds.
    let r = audited(s, actor, "approve", id, note.note.clone(), pending_request(s, id).await).await?;
    let owner = r.spec.owner.clone();
    let client = kube(s)?;
    let api: Api<crd::Quota> = Api::all(client.clone());
    let existing = api.get_opt(&owner).await.map_err(kube_err)?;
    let team = scope::is_team(s, &owner).await;
    let base = match &existing {
        Some(q) => q.spec.clone(),
        None => crate::quota::effective(client, &owner, team).await.map_err(kube_err)?,
    };
    // An operator may grant less or more than asked; absent an edit, approve grants exactly what
    // was requested — unchanged behavior.
    let want = note.requested.clone().unwrap_or_else(|| r.spec.requested.clone());
    audited(s, actor, "approve", &owner, note.note.clone(), write_quota(s, &owner, overlay(base, &want)).await).await?;
    // The grant above is the consequential write; `decide` only marks the request, and if IT
    // fails the quota still landed — the row must survive that, so it's recorded here rather than
    // after the second fallible call.
    audit(s, actor, "approve", &owner, note.note.clone(), "ok").await;
    decide(s, id, crd::RequestState::Approved, actor, note.note.clone()).await
}

/// Deny: mark the request only, no `Quota` write.
async fn deny_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    deny_legacy(&s, &c.name, &id, &legacy_body(&body)?).await
}

async fn deny_legacy(s: &ApiState, actor: &str, id: &str, note: &Decision) -> Result<Response, Response> {
    let r = audited(s, actor, "deny", id, note.note.clone(), pending_request(s, id).await).await?;
    let out = audited(
        s,
        actor,
        "deny",
        &r.spec.owner,
        note.note.clone(),
        decide(s, id, crd::RequestState::Denied, actor, note.note.clone()).await,
    )
    .await?;
    audit(s, actor, "deny", &r.spec.owner, note.note.clone(), "ok").await;
    Ok(out)
}

// ── generic decisions ───────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct GenericDecision {
    #[serde(default)]
    note: Option<String>,
    /// A quota decision's edited grant, replacing `spec.quota` before `overlay` runs — approve
    /// grants what was actually submitted, which is the original ask unless edited.
    #[serde(default)]
    quota: Option<crd::RequestedQuota>,
    /// Required on an `other` approve and free on the rest: an `other` request has nothing to
    /// write, so this sentence IS the decision.
    #[serde(default)]
    resolution: Option<String>,
}

impl GenericDecision {
    /// The legacy shape of the same decision, for an id the migration has not reached yet.
    fn legacy(&self) -> Decision {
        Decision { note: self.note.clone(), requested: self.quota.clone() }
    }
}

fn decision_body(body: &axum::body::Bytes) -> Result<GenericDecision, Response> {
    if body.is_empty() {
        return Ok(GenericDecision::default());
    }
    serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())
}

/// `None` means no `Request` by that name — the caller falls back to the legacy CRD rather than
/// 404ing, because one console queue lists both and every row in it must be decidable.
async fn pending_generic(s: &ApiState, id: &str) -> Result<Option<crd::Request>, Response> {
    check_path_segment(id)?;
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let Some(r) = api.get_opt(id).await.map_err(kube_err)? else {
        return Ok(None);
    };
    if !is_pending_generic(&r) {
        return Err((StatusCode::CONFLICT, "that request has already been decided").into_response());
    }
    Ok(Some(r))
}

/// Stamp the outcome. `status`, not spec: the request is what was asked, the decision is what
/// happened to it, and only this tier ever writes it (no controller reconciles a request).
async fn decide_generic(
    s: &ApiState,
    id: &str,
    state: crd::RequestState,
    by: &str,
    note: Option<String>,
    resolution: Option<String>,
) -> Result<Response, Response> {
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let patch = serde_json::json!({"status": {
        "state": state, "decidedBy": by, "decidedAt": chrono::Utc::now().to_rfc3339(),
        "note": note, "resolution": resolution,
    }});
    let out = api.patch_status(id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(Json(generic_doc(&out)).into_response())
}

/// The base a grant lands on: the owner's own `Quota` if they have one, otherwise the effective
/// defaults — approving must never quietly drop a limit the fallback table already gives them.
async fn quota_base(s: &ApiState, owner: &str) -> Result<crd::QuotaSpec, Response> {
    let client = kube(s)?;
    let api: Api<crd::Quota> = Api::all(client.clone());
    match api.get_opt(owner).await.map_err(kube_err)? {
        Some(q) => Ok(q.spec),
        None => {
            let team = scope::is_team(s, owner).await;
            crate::quota::effective(client, owner, team).await.map_err(kube_err)
        }
    }
}

/// The consequential half of an approve, per kind. Returns the sentence that goes into
/// `status.resolution`: what this decision actually DID.
///
/// Every arm writes its effect BEFORE the request is marked, and the caller audits between the
/// two — if the mark then fails, the grant still landed and the row says so, which is the only
/// order that never claims something that did not happen.
async fn apply_approval(
    s: &ApiState,
    r: &crd::Request,
    d: &GenericDecision,
    actor: &str,
) -> Result<String, Response> {
    let owner = r.spec.owner.clone();
    // Every audit row for a decision names the REQUEST, not the owner: a reader following one
    // decision needs its rows to collate, and the owner is already on the request.
    let id = r.name_any();
    match r.spec.kind {
        crd::RequestKind::Quota => {
            let want = d.quota.clone().or_else(|| r.spec.quota.clone()).unwrap_or_default();
            let base = quota_base(s, &owner).await?;
            audited(
                s,
                actor,
                "request.approved",
                &id,
                d.note.clone(),
                write_quota(s, &owner, overlay(base, &want)).await,
            )
            .await?;
            Ok("quota written".to_string())
        }
        crd::RequestKind::Access => {
            let ask = r.spec.access.clone().ok_or_else(|| {
                // `validate` runs on create, so this is only reachable for an object written
                // around `/v1` — a restored backup, say. Refuse rather than approve a no-op.
                (StatusCode::UNPROCESSABLE_ENTITY, "this access request carries no access block").into_response()
            })?;
            let dir = s.directory.as_ref().ok_or_else(|| {
                (StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response()
            })?;
            // An unknown word is refused, never rounded down to `Member`: the request says what
            // was asked for, and granting something else under an approve is a lie in the record.
            let role = match ask.role.as_str() {
                "owner" => TeamRole::Owner,
                "admin" => TeamRole::Admin,
                "member" => TeamRole::Member,
                other => {
                    return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("unknown role {other}")).into_response())
                }
            };
            // The grant is for the person who ASKED, not for `spec.owner`: an access request's
            // owner is the asker's own slug and the team is what they do not have yet.
            let who = r.spec.requested_by.clone();
            match dir.grant_access(&ask.team, &who, role).await {
                GrantAccess::Done => Ok(format!("{who} is {} of {}", ask.role, ask.team)),
                GrantAccess::NoSuchUser => {
                    Err(audit_refusal(s, actor, &id, d, StatusCode::UNPROCESSABLE_ENTITY, "no such user").await)
                }
                GrantAccess::NoSuchTeam => {
                    Err(audit_refusal(s, actor, &id, d, StatusCode::UNPROCESSABLE_ENTITY, "no such team").await)
                }
                GrantAccess::Refused(why) => {
                    Err(audit_refusal(s, actor, &id, d, StatusCode::CONFLICT, &why).await)
                }
                GrantAccess::Unsupported => Err(audit_refusal(
                    s,
                    actor,
                    &id,
                    d,
                    StatusCode::NOT_IMPLEMENTED,
                    "this process cannot write team membership",
                )
                .await),
            }
        }
        crd::RequestKind::Region => {
            let ask = r.spec.region.clone().ok_or_else(|| {
                (StatusCode::UNPROCESSABLE_ENTITY, "this region request carries no region block").into_response()
            })?;
            // The region must still exist and be active — an approve is a decision somebody will
            // read months from now, and one naming a retired region is worse than a refusal.
            check_region(s, &ask.region).await?;
            let mut base = quota_base(s, &owner).await?;
            if !base.regions.contains(&ask.region) {
                base.regions.push(ask.region.clone());
            }
            audited(s, actor, "request.approved", &id, d.note.clone(), write_quota(s, &owner, base).await).await?;
            Ok(format!("{} recorded as a granted region for {owner}; placement does not read it yet", ask.region))
        }
        crd::RequestKind::Other => {
            let text = d.resolution.as_deref().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err((StatusCode::UNPROCESSABLE_ENTITY, "resolution is required").into_response());
            }
            Ok(text)
        }
    }
}

/// A refusal from a grant is a decision that did NOT happen, and 409-and-up refusals are evidence
/// (same rule as `audited`) — recorded here because the failure is the directory's answer, not a
/// `Result` `audited` could wrap.
async fn audit_refusal(
    s: &ApiState,
    actor: &str,
    id: &str,
    d: &GenericDecision,
    code: StatusCode,
    why: &str,
) -> Response {
    if code.as_u16() >= 409 {
        audit(s, actor, "request.approved", id, d.note.clone(), format!("error:{}", code.as_u16())).await;
    }
    (code, why.to_string()).into_response()
}

async fn approve_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let d = decision_body(&body)?;
    let Some(r) = audited(&s, &c.name, "request.approved", &id, d.note.clone(), pending_generic(&s, &id).await).await?
    else {
        return approve_legacy(&s, &c.name, &id, &d.legacy()).await;
    };
    let resolution = apply_approval(&s, &r, &d, &c.name).await?;
    // The effect above is the consequential write; `decide_generic` only marks the request, and if
    // IT fails the grant still landed — so the row is recorded here rather than after the second
    // fallible call.
    audit(&s, &c.name, "request.approved", &id, d.note.clone(), "ok").await;
    decide_generic(&s, &id, crd::RequestState::Approved, &c.name, d.note, Some(resolution)).await
}

/// Deny: mark the request only, no grant of any kind. The note is required — the asker reads it.
async fn deny_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let d = decision_body(&body)?;
    let note = require_note(d.note.as_deref().unwrap_or(""))?;
    let pending = audited(&s, &c.name, "request.denied", &id, Some(note.clone()), pending_generic(&s, &id).await).await?;
    if pending.is_none() {
        return deny_legacy(&s, &c.name, &id, &d.legacy()).await;
    }
    let out = audited(
        &s,
        &c.name,
        "request.denied",
        &id,
        Some(note.clone()),
        decide_generic(&s, &id, crd::RequestState::Denied, &c.name, Some(note.clone()), None).await,
    )
    .await?;
    audit(&s, &c.name, "request.denied", &id, Some(note), "ok").await;
    Ok(out)
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct RequestFilter {
    owner: Option<String>,
    state: Option<crd::RequestState>,
    /// New in the generic queue; the legacy list ignores it (every legacy row is a quota row, so
    /// any other value simply drops them).
    kind: Option<crd::RequestKind>,
}

/// The whole queue, every owner, narrowable by `?owner=` and `?state=` — `QuotaRequest` carries
/// no label to select on and the fleet-wide row count is small, so filtering here (server-side of
/// this process, client-side of the k3s API) is the honest lazy answer over a new list-selector.
/// Newest first, filtered — the route's own shape, pulled out so Overview can ask for just the
/// pending ones without a second HTTP round trip.
pub(crate) async fn list_all_quota_requests_inner(
    s: &ApiState,
    f: &RequestFilter,
) -> Result<Vec<crd::QuotaRequest>, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(s)?.clone());
    let mut rows = api.list(&ListParams::default()).await.map_err(kube_err)?.items;
    rows.retain(|r| {
        f.owner.as_deref().is_none_or(|o| r.spec.owner == o)
            && f.state.is_none_or(|st| r.status.as_ref().map(|s| s.state).unwrap_or_default() == st)
    });
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(rows)
}

async fn list_all_quota_requests(
    State(s): State<Arc<ApiState>>,
    Query(f): Query<RequestFilter>,
) -> Result<Response, Response> {
    let rows = list_all_quota_requests_inner(&s, &f).await?;
    Ok(Json(rows.iter().map(request_doc).collect::<Vec<_>>()).into_response())
}

/// A legacy `QuotaRequest` wearing the generic doc — the migration has not necessarily run, and a
/// console must never have to know that. `requested` becomes the `quota` block, and everything
/// else it never had stays absent.
fn legacy_doc(r: &crd::QuotaRequest) -> RequestDoc {
    let st = r.status.clone().unwrap_or_default();
    RequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        kind: crd::RequestKind::Quota,
        // The old CRD never recorded an author, and inventing one would be worse than an empty
        // string: the owner is who it was for, not necessarily who typed it.
        requested_by: String::new(),
        reason: r.spec.reason.clone(),
        quota: Some(r.spec.requested.clone()),
        access: None,
        region: None,
        other: None,
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        resolution: None,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

/// The whole queue, both CRDs, newest first. Filtering happens here (server-side of this process,
/// client-side of the k3s API) for the same reason `list_all_quota_requests_inner` does it: the
/// fleet-wide row count is small and neither CRD carries a label to select a kind or a state on.
pub(crate) async fn list_requests_inner(s: &ApiState, f: &RequestFilter) -> Result<Vec<RequestDoc>, Response> {
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let mut rows: Vec<RequestDoc> =
        api.list(&ListParams::default()).await.map_err(kube_err)?.items.iter().map(generic_doc).collect();
    if f.kind.is_none_or(|k| k == crd::RequestKind::Quota) {
        let legacy = RequestFilter { owner: f.owner.clone(), state: f.state, kind: None };
        rows.extend(list_all_quota_requests_inner(s, &legacy).await?.iter().map(legacy_doc));
    }
    rows.retain(|r| {
        f.owner.as_deref().is_none_or(|o| r.owner == o)
            && f.state.is_none_or(|st| r.state == st)
            && f.kind.is_none_or(|k| r.kind == k)
    });
    // `created_at` is an RFC 3339 string, so string order IS time order; an undated row (a
    // just-created object the API server has not stamped) sorts last rather than first.
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(rows)
}

async fn list_requests(State(s): State<Arc<ApiState>>, Query(f): Query<RequestFilter>) -> Result<Response, Response> {
    Ok(Json(list_requests_inner(&s, &f).await?).into_response())
}

/// Copy every legacy `QuotaRequest` into a `Request` of kind quota, once. Idempotent because the
/// new object's NAME is derived from the old one's uid: a re-run collides on create and skips,
/// so nothing has to remember whether this already ran.
///
/// A route rather than a `kl` subcommand or a boot step: it needs the admin process's cluster
/// credentials, it must be auditable, and it must be a decision an operator TAKES rather than a
/// thing that happens to their cluster on a rolling restart. The legacy CRD is retired in a later
/// release; until then the queue unions both anyway, so running this late costs nothing.
async fn migrate_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    // The body carries only an operator's note; nothing here reads it, so it is not recorded
    // beyond the fact that a migration ran (the counts are the reason a reader wants).
    Json(_body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let legacy = list_all_quota_requests_inner(&s, &RequestFilter::default()).await?;
    let api: Api<crd::Request> = Api::all(kube(&s)?.clone());
    let (mut copied, mut skipped) = (0u32, 0u32);
    for old in &legacy {
        let Some(uid) = old.metadata.uid.as_deref() else {
            // No uid means the object never reached etcd — nothing to copy, and no stable name
            // to make the copy idempotent under.
            skipped += 1;
            continue;
        };
        let name = format!("q-{uid}");
        // A migrated copy carries no status, so it reads as pending; a legacy request that was
        // already decided must not reappear in the pending queue.
        let decided = old.status.as_ref().filter(|st| st.state != crd::RequestState::Pending);
        let mut r = crd::Request::new(
            &name,
            crd::RequestSpec {
                owner: old.spec.owner.clone(),
                kind: crd::RequestKind::Quota,
                // The old CRD never recorded an author; the owner is who it was for.
                requested_by: old.spec.owner.clone(),
                reason: old.spec.reason.clone(),
                quota: Some(old.spec.requested.clone()),
                access: None,
                region: None,
                other: None,
            },
        );
        r.metadata.labels =
            Some(std::collections::BTreeMap::from([(OWNER_LABEL.to_string(), old.spec.owner.clone())]));
        match api.create(&PostParams::default(), &r).await {
            Ok(_) => copied += 1,
            // Already migrated. The one error this loop swallows on purpose — it is the idempotence.
            Err(kube::Error::Api(e)) if e.code == 409 => {
                skipped += 1;
                // The create and the status stamp are two calls, so an earlier run could have
                // died between them and left a DECIDED legacy request pending forever in the new
                // queue. Only then is the copy worth re-reading, and only while it is still
                // pending is it worth stamping — a copy already decided, or since deleted, is
                // left exactly as it stands.
                if decided.is_none() {
                    continue;
                }
                let Some(copy) = api.get_opt(&name).await.map_err(kube_err)? else { continue };
                if copy.status.is_some_and(|st| st.state != crd::RequestState::Pending) {
                    continue;
                }
            }
            Err(e) => return Err(kube_err(e)),
        }
        if let Some(st) = decided {
            decide_generic(&s, &name, st.state, st.decided_by.as_deref().unwrap_or(""), st.note.clone(), None).await?;
        }
    }
    // The copies already landed; the row goes in before the response is built.
    audit(&s, &c.name, "requests.migrated", "quotarequests", Some(format!("copied={copied} skipped={skipped}")), "ok")
        .await;
    Ok(Json(serde_json::json!({"copied": copied, "skipped": skipped})).into_response())
}

// ── nodes ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeDoc {
    pub(crate) name: String,
    pub(crate) ready: bool,
    pub(crate) decommission: bool,
    pub(crate) decommission_status: Option<String>,
}

/// Every node of one cluster, read fresh — `GET /admin/nodes` and the Clusters area both compose
/// from this rather than each deciding for itself what "ready" or "draining" means.
pub(crate) async fn node_docs(client: &kube::Client) -> Result<Vec<NodeDoc>, Response> {
    let api: Api<k8s_openapi::api::core::v1::Node> = Api::all(client.clone());
    Ok(api.list(&ListParams::default()).await.map_err(kube_err)?.items.iter().map(node_doc).collect())
}

pub(crate) fn node_doc(n: &k8s_openapi::api::core::v1::Node) -> NodeDoc {
    let ready = n
        .status
        .as_ref()
        .and_then(|st| st.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True");
    let labels = n.metadata.labels.clone().unwrap_or_default();
    let annotations = n.metadata.annotations.clone().unwrap_or_default();
    NodeDoc {
        name: n.name_any(),
        ready,
        // Only the exact value counts, same rule the agent's own `decommissioning` applies.
        decommission: labels.get(crd::DECOMMISSION_LABEL).map(String::as_str) == Some("true"),
        decommission_status: annotations.get(crd::DECOMMISSION_STATUS).cloned(),
    }
}

async fn list_nodes(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    Ok(Json(node_docs(kube(&s)?).await?).into_response())
}

// ── cross-owner list / stop / delete ────────────────────────────────────────
//
// Workspaces: the SAME handler `api::workspaces` exports for the owner-scoped `/v1` route
// (`list_for_owner`/`stop_as`/`delete_as`), called with the owner taken from a query param or the
// object itself rather than the caller — `my_ws`'s superadmin arm is what makes that legitimate.
//
// Environments' `stop_env`/`delete_env` are already generic (owner from `find_env`, superadmin
// admitted by `may_act_on`'s claim arm), so the wrappers below add exactly one thing each: the
// required note and the audit row. Acting on somebody ELSE's workspace is the loudest thing this
// console does, so it is the one place a wrapper earns its existence. `list_env` is a read and is
// still mounted directly.

#[derive(serde::Deserialize)]
struct OwnerQuery {
    owner: String,
}

async fn admin_list_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<OwnerQuery>,
) -> Result<Response, Response> {
    super::workspaces::list_for_owner(&s, &headers, &q.owner).await
}

async fn admin_stop_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = require_note(&body.note)?;
    let out = audited(&s, &c.name, "stop-workspace", &id, Some(note.clone()), super::workspaces::stop_as(&s, &headers, &id).await)
        .await?;
    audit(&s, &c.name, "stop-workspace", &id, Some(note), "ok").await;
    Ok(out)
}

async fn admin_delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = require_note(&body.note)?;
    let out =
        audited(&s, &c.name, "delete-workspace", &id, Some(note.clone()), super::workspaces::delete_as(&s, &headers, &id).await)
            .await?;
    audit(&s, &c.name, "delete-workspace", &id, Some(note), "ok").await;
    Ok(out)
}

async fn admin_stop_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = require_note(&body.note)?;
    let r = super::environments::stop_env(State(s.clone()), headers.clone(), Path(id.clone())).await;
    let out = audited(&s, &c.name, "stop-environment", &id, Some(note.clone()), r).await?;
    audit(&s, &c.name, "stop-environment", &id, Some(note), "ok").await;
    Ok(out)
}

async fn admin_delete_env(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = require_note(&body.note)?;
    let r = super::environments::delete_env(State(s.clone()), headers.clone(), Path(id.clone())).await;
    let out = audited(&s, &c.name, "delete-environment", &id, Some(note.clone()), r).await?;
    audit(&s, &c.name, "delete-environment", &id, Some(note), "ok").await;
    Ok(out)
}

// ── workload rolls ──────────────────────────────────────────────────────

/// `central`, or a region id — the same encoding `POST /admin/workloads/{scope}/{name}/roll`'s
/// one path segment uses to name either half of `KNOWN`.
pub(crate) fn parse_scope(seg: &str) -> super::workloads::Scope {
    if seg == "central" { super::workloads::Scope::Central } else { super::workloads::Scope::Region(seg.to_string()) }
}

/// Every active region — the source `list_workloads`' per-region half walks, and `api::settings`'
/// central-scope boot roll (`sshHost`/`sshPort` → every region's gateway) walks the same list.
pub(crate) async fn active_regions(s: &ApiState) -> Result<Vec<String>, Response> {
    let api: Api<crd::Region> = Api::all(kube(s)?.clone());
    Ok(api
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.status == "active")
        .map(|r| r.name_any())
        .collect())
}

async fn list_workloads_route(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let regions = active_regions(&s).await?;
    let rows = super::workloads::list_workloads(&s, &regions).await?;
    Ok(Json(rows).into_response())
}

#[derive(serde::Deserialize)]
struct RollBody {
    #[serde(default)]
    reason: String,
}

/// The manual route's one validation beyond what the boot-triggered path needs: a boot roll's
/// reason is always `setting:<field>`, this one is free text a human typed, so it must not be
/// empty.
async fn roll_workload_route(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((scope, name)): Path<(String, String)>,
    Json(body): Json<RollBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let reason = body.reason.trim().to_string();
    if reason.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "reason is required").into_response());
    }
    let target = format!("{scope}/{name}");
    let scope = parse_scope(&scope);
    audited(
        &s,
        &c.name,
        "roll",
        &target,
        Some(reason.clone()),
        super::workloads::roll_readers(&s, &scope, &[name.as_str()], super::workloads::RollReason::Manual(reason.clone()), &c.name)
            .await,
    )
    .await?;
    // The roll already happened; `workload_doc` below is only a read for the response, so a row
    // must land before it in case that read fails.
    audit(&s, &c.name, "roll", &target, Some(reason), "ok").await;
    let row = super::workloads::workload_doc(&s, &scope, &name).await?;
    Ok(Json(row).into_response())
}
