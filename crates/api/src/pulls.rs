use super::*;

// ── pull requests ───────────────────────────────────────────────────────────
//
// A PR is metadata pointing at two BRANCHES. It stores no commits and no diff:
// those are computed from the refs on every read, so a push to the branch updates
// what the PR contains — which is what review is. Storing a snapshot would mean a
// PR that can disagree with the code it claims to be about.


pub(crate) async fn open_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(mut body): axum::Json<serde_json::Value>,
) -> Response {
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // The author is WHO IS SIGNED IN, never what the request said — the owning node has no idea
    // who the caller is, so a body that could name its own author would let anyone open a change
    // as somebody else. Everything else is passed through as handed to us.
    let Some(obj) = body.as_object_mut() else {
        return (StatusCode::BAD_REQUEST, "expected an object").into_response();
    };
    obj.insert("author".into(), serde_json::Value::String(who.email));
    tell_owner(&api, &owner, format!("/api/{}/{}/pulls", encode(&owner), encode(&name)), body).await
}

pub(crate) async fn list_pulls(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let mut path = format!("/api/{}/{}/pulls", encode(&owner), encode(&name));
    let mut sep = '?';
    for k in ["state", "limit"] {
        if let Some(v) = q.get(k) {
            path.push(sep);
            sep = '&';
            path.push_str(k);
            path.push('=');
            path.push_str(&encode(v));
        }
    }
    read_from_owner(&api, &owner, path).await
}

pub(crate) async fn get_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    read_from_owner(
        &api,
        &owner,
        format!("/api/{}/{}/pulls/{number}", encode(&owner), encode(&name)),
    )
    .await
}

pub(crate) async fn comment_on_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
    axum::Json(mut body): axum::Json<serde_json::Value>,
) -> Response {
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Same reason as `open_pull`: the signed-in caller names the author, not the body.
    let Some(obj) = body.as_object_mut() else {
        return (StatusCode::BAD_REQUEST, "expected an object").into_response();
    };
    obj.insert("author".into(), serde_json::Value::String(who.email));
    tell_owner(
        &api,
        &owner,
        format!("/api/{}/{}/pulls/{number}/comments", encode(&owner), encode(&name)),
        body,
    )
    .await
}

/// What a branch would bring to another. Straight through to the owning node —
/// this is a read of git, not of the directory.
pub(crate) async fn compare_branches(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let (Some(base), Some(head)) = (q.get("base"), q.get("head")) else {
        return (StatusCode::BAD_REQUEST, "base and head are required").into_response();
    };
    read_from_owner(
        &api,
        &owner,
        format!(
            "/api/{}/{}/compare?base={}&head={}",
            encode(&owner),
            encode(&name),
            encode(base),
            encode(head)
        ),
    )
    .await
}

/// Ask for the change to be merged.
///
/// Answers 202, not 200: the merge is a JOB. It can be slow — a three-way merge
/// on a large tree is real work — and running it inside this request would hold a
/// connection open on a git node that is also serving clones. The worker picks it
/// up; the PR reports where it got to.
pub(crate) async fn merge_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let strategy = match q.get("strategy").map(String::as_str).unwrap_or("fast-forward") {
        s @ ("fast-forward" | "squash" | "merge" | "rebase") => s,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "strategy must be fast-forward, squash, merge or rebase",
            )
                .into_response()
        }
    };

    // Forwarded like the close beneath it: the change lives in the repo's own database, and the
    // node publishes the event. 202, not 200 — the merge is a JOB, and running it inside this
    // request would hold a connection open on a node that is also serving clones.
    let path = format!(
        "/api/{}/{}/pulls/{number}/merge?strategy={}&by={}",
        encode(&owner),
        encode(&name),
        encode(strategy),
        encode(&who.email)
    );
    match ask_owner(&api, path).await {
        Ok(200..=299) => (StatusCode::ACCEPTED, "merging").into_response(),
        // Not open, or a merge is already in flight. Asking twice must not queue
        // it twice, and saying so is more use than a second "accepted".
        Ok(409) => (
            StatusCode::CONFLICT,
            "this change is not open, or a merge is already under way",
        )
            .into_response(),
        Ok(404) => not_found(),
        Ok(s) => {
            tracing::error!(owner = %owner, name = %name, number = number, status = s, "request merge: unexpected upstream status");
            (StatusCode::BAD_GATEWAY, "could not ask for the merge").into_response()
        }
        Err(r) => r,
    }
}

pub(crate) async fn close_pull(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, number)): axum::extract::Path<(String, String, i64)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Forwarded, not written here: the change lives in the repo's own database, and only the
    // owning node may touch it. That handler publishes the event too, so this tier is left with
    // the one question it alone can answer — may this person close it.
    let path = format!(
        "/api/{}/{}/pulls/{number}/close?by={}",
        encode(&owner),
        encode(&name),
        encode(&who.email)
    );
    match ask_owner(&api, path).await {
        Ok(200..=299) => StatusCode::NO_CONTENT.into_response(),
        Ok(409) => (StatusCode::CONFLICT, "this change is not open").into_response(),
        Ok(404) => not_found(),
        Ok(s) => {
            tracing::error!(owner = %owner, name = %name, number = number, status = s, "close pull: unexpected upstream status");
            (StatusCode::BAD_GATEWAY, "could not close the change").into_response()
        }
        Err(r) => r,
    }
}
