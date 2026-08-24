use super::*;

/// Buffer an upstream reply, refusing anything past `MAX_BODY` instead of holding it in memory.
/// Hand-synced twin in `bins/server/src/boot.rs` (`post_to_owner`) — mirror any change there.
pub async fn read_bounded(mut r: reqwest::Response) -> Result<axum::body::Bytes> {
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(crate::err("upstream reply is too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.into())
}

/// `read_bounded`, as the text a handler relays. An oversized reply is an empty string, which the
/// relaying status code already explains better than a truncated body would.
pub(crate) async fn text_bounded(r: reqwest::Response) -> String {
    read_bounded(r)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Ask the node that owns this repo to do something. Every settings change goes
/// through here, so they all present the peer secret the same way.
pub(crate) async fn ask_owner(api: &Api, path: String) -> std::result::Result<u16, Response> {
    let url = format!("{}{path}", api.upstream);
    match api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .send()
        .await
    {
        Ok(r) => Ok(r.status().as_u16()),
        Err(e) => {
            eprintln!("settings upstream: {e}"); // ponytail: eprintln
            Err((StatusCode::BAD_GATEWAY, "the service is unavailable").into_response())
        }
    }
}


/// Read something from the node that owns this repo, and pass its answer through.
/// Read a repo-scoped route from the owning node, as `owner`.
///
/// The peer secret is not an identity. It says "a node in this fleet is asking",
/// and the node still applies the same read check it applies to anyone — so it
/// has to be told WHO is reading, or a private repo answers 401 to a caller who
/// is entitled to it. The caller establishes that entitlement before calling
/// this; `owner` is what it asserts upstream.
pub(crate) async fn read_from_owner(api: &Api, owner: &str, path: String) -> Response {
    let url = format!("{}{path}", api.upstream);
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, owner)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match read_bounded(r).await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => {
            eprintln!("upstream body: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
        }
    }
}

/// Forward a JSON body to the owning node, as `owner`, and pass its answer straight back.
///
/// The sibling of `ask_owner` for the two PR writes that carry real user text. The node's own
/// refusals ("a title is required", "say something") are written for the person typing, so they
/// are relayed rather than replaced — the same choice `commit_patch` makes for its forward.
pub(crate) async fn tell_owner(api: &Api, owner: &str, path: String, body: serde_json::Value) -> Response {
    let url = format!("{}{path}", api.upstream);
    let sent = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, owner)
        .json(&body)
        .send()
        .await;
    let r = match sent {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pull upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = text_bounded(r).await;
    if status.is_success() {
        (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
    } else {
        (status, text).into_response()
    }
}

