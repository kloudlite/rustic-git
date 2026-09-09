//! `/v1/quota` and `/v1/requests`: the owner's effective quota and usage, the one pending ask
//! per owner per kind (quota / access / region / other), and the retired `QuotaRequest` still
//! read alongside it.

use super::*;


#[derive(serde::Deserialize)]
pub(crate) struct QuotaQuery {
    /// Absent means the caller's own. A team slug they belong to is allowed; anything else is a
    /// 404, same as every other owner-scoped read.
    #[serde(default)]
    owner: Option<String>,
}


/// `GET /v1/quota?owner=` — the ceiling and what is against it, both for one owner.
///
/// Usage is computed here and nowhere else, on every request (see `quota::usage`'s module doc).
pub(crate) async fn get_quota(
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
pub(crate) struct NewQuotaRequest {
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
pub(crate) async fn may_request_for(s: &ApiState, caller: &str, owner: &str) -> Result<(), Response> {
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
pub(crate) async fn requests_of(c: &kube::Client, owner: &str) -> Result<Vec<crd::QuotaRequest>, Response> {
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
pub(crate) fn is_pending(r: &crd::QuotaRequest) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}


/// The pre-`Request` route, kept because the web's 409 dialog and `kl` both post here. It writes a
/// kind-quota `Request` now: one queue, one pending rule, one decision path — the old CRD is only
/// ever READ from here on.
pub(crate) async fn create_quota_request(
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
pub(crate) struct QuotaRequestDoc {
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


pub(crate) fn request_doc(r: &crd::QuotaRequest) -> QuotaRequestDoc {
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
pub(crate) struct RequestQuery {
    #[serde(default)]
    owner: Option<String>,
}


/// The caller's own requests and their teams'. `owner` narrows to one, and must be something the
/// caller may act on — same rule as every other owner-scoped read.
pub(crate) async fn list_quota_requests(
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


/// No status yet is PENDING — `/v1` writes the object and stamps status in a second call, and
/// reading that window as "decided" would let two requests of one kind stand at once.
pub(crate) fn is_pending_generic(r: &crd::Request) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}


#[derive(serde::Deserialize)]
pub(crate) struct NewRequest {
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


pub(crate) async fn create_request(
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
pub(crate) async fn list_requests(
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


pub(crate) async fn get_request(
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
pub(crate) struct Decision {
    #[serde(default)]
    pub(crate) note: Option<String>,
    /// The operator's edited ask, replacing `r.spec.requested` before `overlay` runs — approve
    /// grants what was actually submitted, which is the original request unless edited.
    #[serde(default)]
    pub(crate) requested: Option<crd::RequestedQuota>,
}
