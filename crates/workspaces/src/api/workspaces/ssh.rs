//! `POST /v1/workspaces/{id}/ssh`: the one-shot session ticket `kl-connect ws ssh` presents to
//! the region gateway.

use super::*;


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
        tracing::error!(error = %e, "ssh.session.mint.failed");
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
