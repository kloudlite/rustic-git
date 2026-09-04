use super::*;

/// `read_bounded`, as the text a handler relays. An oversized reply is an empty string, which the
/// relaying status code already explains better than a truncated body would.
pub(crate) async fn text_bounded(r: reqwest::Response) -> String {
    kloudlite_git_core::httpx::read_bounded(r)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// The one way this tier talks to the node that owns a repo: present the peer secret, name the
/// reader, send, and turn an unreachable node into one 502 everywhere.
///
/// The peer secret is not an identity. It says "a node in this fleet is asking", and the node
/// still applies the same read check it applies to anyone — so it has to be told WHO is reading,
/// or a private repo answers 401 to a caller who is entitled to it. The caller establishes that
/// entitlement before calling this; `owner` is what it asserts upstream. `None` is for the writes
/// the node authorizes structurally rather than per reader.
pub(crate) async fn to_owner(
    api: &Api,
    req: reqwest::RequestBuilder,
    owner: Option<&str>,
) -> std::result::Result<reqwest::Response, Response> {
    let req = req.header(kloudlite_git_core::peer::PEER_HEADER, &api.secret);
    let req = match owner {
        Some(o) => req.header(kloudlite_git_core::peer::OWNER_HEADER, o),
        None => req,
    };
    req.send().await.map_err(|e| {
        tracing::error!(error = %e, "upstream.request.failed");
        (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
    })
}

/// Ask the node that owns this repo to do something; the caller reads the outcome off the status.
pub(crate) async fn ask_owner(api: &Api, path: String) -> std::result::Result<u16, Response> {
    let r = to_owner(api, api.client.post(format!("{}{path}", api.upstream)), None).await?;
    Ok(r.status().as_u16())
}

/// Read a repo-scoped route from the owning node, as `owner`, and pass its answer through.
pub(crate) async fn read_from_owner(api: &Api, owner: &str, path: String) -> Response {
    match to_owner(api, api.client.get(format!("{}{path}", api.upstream)), Some(owner)).await {
        Ok(r) => relay(r).await,
        Err(r) => r,
    }
}

/// Forward a JSON body to the owning node, as `owner`, and pass its answer straight back.
///
/// The sibling of `ask_owner` for the two PR writes that carry real user text. The node's own
/// refusals ("a title is required", "say something") are written for the person typing, so they
/// are relayed rather than replaced — the same choice `commit_patch` makes for its forward.
pub(crate) async fn tell_owner(api: &Api, owner: &str, path: String, body: serde_json::Value) -> Response {
    let req = api.client.post(format!("{}{path}", api.upstream)).json(&body);
    match to_owner(api, req, Some(owner)).await {
        Ok(r) => relay(r).await,
        Err(r) => r,
    }
}

/// Pass an upstream reply through with its own status. Only a success body is JSON — a refusal is
/// the node's own prose, and labelling that `application/json` makes it unreadable to the caller.
pub(crate) async fn relay(r: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = text_bounded(r).await;
    if status.is_success() {
        (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
    } else {
        (status, text).into_response()
    }
}
