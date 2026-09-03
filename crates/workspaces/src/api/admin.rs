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
        .route("/admin/quota-requests", get(list_all_quota_requests))
        .route("/admin/quota-requests/{id}/approve", post(approve_quota_request))
        .route("/admin/quota-requests/{id}/deny", post(deny_quota_request))
        .route("/admin/usage", get(usage_all))
        .route("/admin/nodes", get(list_nodes))
        .route("/admin/workspaces", get(admin_list_ws))
        .route("/admin/workspaces/{id}", axum::routing::delete(admin_delete_ws))
        .route("/admin/workspaces/{id}/stop", post(admin_stop_ws))
        .route("/admin/environments", get(super::environments::list_env))
        .route("/admin/environments/{id}", axum::routing::delete(super::environments::delete_env))
        .route("/admin/environments/{id}/stop", post(super::environments::stop_env))
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
}

fn active_status() -> String {
    "active".into()
}

async fn create_region(
    State(s): State<Arc<ApiState>>,
    Json(body): Json<NewRegion>,
) -> Result<Response, Response> {
    // The claim already gated this request in `refuse_without_claim`; no second check here — the
    // one place that decides is the layer every route on this router shares.
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

async fn write_quota_route(
    State(s): State<Arc<ApiState>>,
    Path(owner): Path<String>,
    Json(spec): Json<crd::QuotaSpec>,
) -> Result<Response, Response> {
    let q = write_quota(&s, &owner, spec).await?;
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
    let note: Decision = if body.is_empty() { Default::default() } else {
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())?
    };
    let r = pending_request(&s, &id).await?;
    let owner = r.spec.owner.clone();
    let client = kube(&s)?;
    let api: Api<crd::Quota> = Api::all(client.clone());
    let existing = api.get_opt(&owner).await.map_err(kube_err)?;
    let team = scope::is_team(&s, &owner).await;
    let base = match &existing {
        Some(q) => q.spec.clone(),
        None => crate::quota::effective(client, &owner, team).await.map_err(kube_err)?,
    };
    write_quota(&s, &owner, overlay(base, &r.spec.requested)).await?;
    decide(&s, &id, crd::RequestState::Approved, &c.name, note.note).await
}

/// Deny: mark the request only, no `Quota` write.
async fn deny_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note: Decision = if body.is_empty() { Default::default() } else {
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())?
    };
    pending_request(&s, &id).await?;
    decide(&s, &id, crd::RequestState::Denied, &c.name, note.note).await
}

/// The whole queue, every owner — the admin list has no `owner` filter, unlike `/v1`'s.
async fn list_all_quota_requests(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(&s)?.clone());
    let mut rows = api.list(&ListParams::default()).await.map_err(kube_err)?.items;
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(request_doc).collect::<Vec<_>>()).into_response())
}

// ── usage across every owner ────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct OwnerUsage {
    owner: String,
    limit: crd::QuotaSpec,
    used: crate::quota::Usage,
}

/// ponytail: the owner list is derived from who has an explicit `Quota` or has ever opened a
/// `QuotaRequest` — an owner using only the defaults and who has never asked for more is not
/// listed. A `Node`-free way to enumerate every owner would need a third index (every distinct
/// `rustic-git.io/owner` label value); add one if the admin usage page has to be exhaustive.
async fn usage_all(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let client = kube(&s)?;
    let quotas: Api<crd::Quota> = Api::all(client.clone());
    let reqs: Api<crd::QuotaRequest> = Api::all(client.clone());
    let mut owners: std::collections::BTreeSet<String> = quotas
        .list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter()
        .map(|q| q.name_any())
        .filter(|n| n != crd::DEFAULT_USER_QUOTA && n != crd::DEFAULT_TEAM_QUOTA)
        .collect();
    owners.extend(reqs.list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(|r| r.spec.owner));
    let mut rows = Vec::new();
    for owner in owners {
        // ponytail: whether `owner` is a team decides which default column applies, and the only
        // honest answer needs the directory; without one every row falls back to the person
        // default rather than 503ing the whole page over one lookup.
        let team = scope::is_team(&s, &owner).await;
        let limit = crate::quota::effective(client, &owner, team).await.map_err(kube_err)?;
        let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
        rows.push(OwnerUsage { owner, limit, used });
    }
    Ok(Json(rows).into_response())
}

// ── nodes ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeDoc {
    name: String,
    ready: bool,
    decommission: bool,
    decommission_status: Option<String>,
}

async fn list_nodes(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let api: Api<k8s_openapi::api::core::v1::Node> = Api::all(kube(&s)?.clone());
    let rows: Vec<NodeDoc> = api
        .list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter()
        .map(|n| {
            let ready = n.status.as_ref()
                .and_then(|st| st.conditions.as_ref())
                .into_iter().flatten()
                .any(|c| c.type_ == "Ready" && c.status == "True");
            let labels = n.metadata.labels.clone().unwrap_or_default();
            let annotations = n.metadata.annotations.clone().unwrap_or_default();
            NodeDoc {
                name: n.name_any(),
                ready,
                decommission: labels.get("rustic-git.io/decommission").map(String::as_str) == Some("true"),
                decommission_status: annotations.get("rustic-git.io/decommission-status").cloned(),
            }
        })
        .collect();
    Ok(Json(rows).into_response())
}

// ── cross-owner list / stop / delete ────────────────────────────────────────
//
// Workspaces: the SAME handler `api::workspaces` exports for the owner-scoped `/v1` route
// (`list_for_owner`/`stop_as`/`delete_as`), called with the owner taken from a query param or the
// object itself rather than the caller — `my_ws`'s superadmin arm is what makes that legitimate.
//
// Environments are ALREADY generic: `list_env`/`stop_env`/`delete_env` take the owner from
// `?owner=`/`find_env` and already admit a superadmin (`may_act_on`'s claim arm), so they are
// mounted directly above with no wrapper at all — a wrapper that only forwarded its arguments
// would be a function whose only job is to exist.

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
) -> Result<Response, Response> {
    super::workspaces::stop_as(&s, &headers, &id).await
}

async fn admin_delete_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    super::workspaces::delete_as(&s, &headers, &id).await
}
