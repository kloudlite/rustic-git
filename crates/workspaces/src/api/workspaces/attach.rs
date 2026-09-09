//! Attaching a workspace to one environment and detaching it: the spec field only `/v1` writes,
//! and the environment-side NetworkPolicy this handler removes itself on detach and delete.

use super::*;


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
