use super::*;

/// `GET /api/{owner}/images` — the Container Images page. Proxied by hand rather than through
/// `handle`: that path only ever names a repo, and this one names no repo at all, so it does not
/// fit `split_api_path`'s three-segment shape. No caching either — unlike a repo's browse routes,
/// there is no single visibility flag to key a cache entry on; the answer is small and per-team.
pub(crate) async fn images_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path(owner): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    if !crate::store::valid_segment(&owner) {
        return not_found();
    }
    let caller = match browse_caller(&api, &headers, &owner).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    // A team's images are never a stranger's business, and there is no "public team" concept —
    // unlike a repo, which can admit an anonymous reader. Only a verified member of `owner` passes.
    let anonymous = caller.is_none();
    let Some(who) = caller.filter(|c| c == &owner) else {
        return if anonymous { unauthorized() } else { not_found() };
    };
    let url = format!("{}/api/{}/images", api.upstream, encode(&owner));
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &who)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "upstream");
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    relay(r).await
}

/// `POST /api/{owner}/{image}/imagetagdelete` — proxied by hand for the same reason
/// `images_proxy` is: it is a write, and the fallback below only ever forwards a GET.
///
/// The body (the tag name) is forwarded verbatim to the node that owns the image's database.
pub(crate) async fn imagetagdelete_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    image_write_proxy(&api, &owner, &image, "imagetagdelete", &headers, Some(body), None).await
}

/// `POST /api/{owner}/{image}/imagedelete` — same shape as `imagetagdelete_proxy`, no body.
pub(crate) async fn imagedelete_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    image_write_proxy(&api, &owner, &image, "imagedelete", &headers, None, None).await
}

/// `POST /api/{owner}/{image}/imagevisibility?visibility=public|private`.
///
/// The visibility value is PARSED here and re-emitted, so only `public` or `private` ever reaches
/// upstream — the node would reject anything else anyway, but a 400 belongs where the caller can
/// read it.
pub(crate) async fn imagevisibility_proxy(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, image)): axum::extract::Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let visibility = match q.get("visibility").map(String::as_str) {
        Some("public") => "public",
        Some("private") => "private",
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    image_write_proxy(&api, &owner, &image, "imagevisibility", &headers, None,
        Some(&format!("visibility={visibility}"))).await
}

/// Shared by both image writes: authorize the caller as exactly `owner` (an image, like a team's
/// image list, is never a stranger's business — there is no public-image concept to fall back on),
/// then forward to the upstream node the same way `images_proxy` reads from it.
pub(crate) async fn image_write_proxy(
    api: &Api,
    owner: &str,
    image: &str,
    tail: &str,
    headers: &HeaderMap,
    body: Option<axum::body::Bytes>,
    // Rebuilt from the parsed value, never forwarded raw: the upstream route reads
    // `?visibility=`, and passing a caller-supplied string through unchecked is how a query
    // becomes a second parser. `None` for the tails that take no query.
    query: Option<&str>,
) -> Response {
    if !crate::store::valid_segment(owner) || !crate::store::valid_segment(image) {
        return not_found();
    }
    let caller = match browse_caller(api, headers, owner).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let anonymous = caller.is_none();
    let Some(who) = caller.filter(|c| c == owner) else {
        return if anonymous { unauthorized() } else { not_found() };
    };
    let url = match query {
        Some(q) => format!("{}/api/{}/{}/{tail}?{q}", api.upstream, encode(owner), encode(image)),
        None => format!("{}/api/{}/{}/{tail}", api.upstream, encode(owner), encode(image)),
    };
    let mut up = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &who);
    if let Some(b) = body {
        up = up.body(b);
    }
    let r = match up.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "upstream");
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    relay(r).await
}
