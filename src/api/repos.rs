use super::*;

// ── repos ───────────────────────────────────────────────────────────────────

/// A repo as the web sees it.
///
/// The stored `createdAt` is a BSON date, which serde renders as
/// `{"$date":{"$numberLong":"…"}}` — an encoding a browser has no business
/// parsing. The wire shape is milliseconds, which `new Date(n)` reads directly.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepoOut {
    #[serde(rename = "_id")]
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) public: bool,
    pub(crate) description: String,
    pub(crate) created_by: String,
    pub(crate) created_at: i64,
}

/// An owner's repos for listing, from the listing-index markers rather than the Mongo mirror.
///
/// The markers ARE the listing truth now (spec §6): they are plain object-store keys, so this
/// answers on any node without opening a single repo database, and it cannot disagree with a row
/// that a failed write left behind. `_id` is not lost by leaving Mongo — it always was
/// `owner/name`, which the marker's path already carries.
///
/// `include_private` is the whole security surface: `index::list` only withholds private names
/// when it is `false`, so a caller whose membership has NOT been established must never reach
/// here with `true` — the same contract `image_listing` states for images.
///
/// Newest first, as the Mongo `sort(createdAt: -1)` this replaces was, so the page does not
/// reorder itself at the cutover.
pub(crate) async fn repo_listing(api: &Api, owner: &str, include_private: bool) -> Result<Vec<RepoOut>> {
    let markers =
        crate::index::list(&api.store.os, crate::index::Kind::Repo, owner, include_private).await?;
    let mut out: Vec<RepoOut> = markers
        .into_iter()
        .map(|m| RepoOut {
            id: format!("{owner}/{}", m.name),
            owner: owner.to_string(),
            name: m.name,
            public: m.public,
            description: m.description,
            created_by: m.created_by,
            created_at: m.created_ms,
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

#[derive(serde::Deserialize)]
pub(crate) struct NewRepo {
    /// The namespace: the caller's own handle, or a team they belong to.
    owner: String,
    name: String,
    /// Absent means private, matching the node route it forwards to.
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    description: String,
}

/// May `user` (an email) create under `owner`?
///
/// Two ways to qualify and no third: it is their own handle, or they are a member
/// of the team of that name. Roles are not distinguished — a member who cannot
/// create a repo is a member who cannot do the work — but membership is required,
/// so holding a session is never on its own enough to write into a namespace.
///
/// A team that does not exist and a team the caller is not in give the same
/// answer, so this cannot be used to enumerate teams.
pub(crate) async fn may_act_under(
    db: &crate::directory::Directory,
    user: &str,
    owner: &str,
) -> Result<bool> {
    if let Some(u) = db.user(user).await? {
        if u.username.as_deref() == Some(owner) {
            return Ok(true);
        }
    }
    Ok(db
        .get(owner)
        .await?
        .is_some_and(|t| t.members.iter().any(|m| m.user.eq_ignore_ascii_case(user))))
}

pub(crate) async fn create_repo(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewRepo>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (owner, name) = (body.owner.trim(), body.name.trim());
    let visibility = match body.visibility.as_deref() {
        None | Some("private") => "private",
        Some("public") => "public",
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    // Validated HERE as well as on the node: this builds a URL from these two
    // strings, and a name carrying a slash or a dot segment would address a
    // different route than the one authorized just above.
    if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
        return (StatusCode::BAD_REQUEST, "invalid repository name").into_response();
    }
    if crate::store::reserved_repo_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("`{name}` is a page in this namespace, so a repository cannot be called it"),
        )
            .into_response();
    }
    // After the request has been judged on its own terms: a malformed name is
    // refused the same way whether or not the database happens to be reachable.
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        // Not 403: whether a team exists is not this caller's business to learn.
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not create repository").into_response();
        }
    }

    // The name is claimed by the CREATE itself, on the node that owns the repo. There is nothing
    // to reserve here first: both creates of one name route to that same node by repo key, so its
    // check-then-create is the single writer that decides uniqueness, and a 409 from it is that
    // decision. Reserving a row here as well would only add a second thing to unwind.
    let repo = RepoOut {
        id: format!("{owner}/{name}"),
        owner: owner.to_string(),
        name: name.to_string(),
        public: visibility == "public",
        description: body.description.trim().to_string(),
        created_by: user.clone(),
        created_at: crate::ownership::now_ms() as i64,
    };

    // The description and creator travel as query parameters because this route takes no body:
    // the owning node writes them into the repo's own database, and the same `created_at_ms` is
    // echoed back to the caller so the two records name the same moment.
    let url = format!(
        "{}/api/{}/{}/create?visibility={visibility}&description={}&created_by={}&created_at_ms={}",
        api.upstream,
        encode(owner),
        encode(name),
        encode(&repo.description),
        encode(&repo.created_by),
        repo.created_at,
    );
    let sent = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .send()
        .await;
    let status = match &sent {
        Ok(r) => r.status().as_u16(),
        Err(e) => {
            eprintln!("create repo upstream: {e}"); // ponytail: eprintln
            0
        }
    };
    match status {
        201 | 204 => (StatusCode::CREATED, axum::Json(repo)).into_response(),
        // The owning node's answer that the name is taken, rendered as the same refusal callers
        // have always had for it.
        409 => (StatusCode::CONFLICT, "a repository of that name already exists").into_response(),
        other => {
            // The create got far enough to claim the name and then failed — or failed before the
            // claim, in which case this delete is a no-op. Either way the name must not outlive
            // this request, otherwise it is held by nothing and the person who tried to create it
            // cannot try again.
            let path = format!("/api/{}/{}/delete", encode(owner), encode(name));
            // Best effort, and its own failure is already logged by `ask_owner`: this request is
            // being refused either way, and the owning node's structural sweep is what catches a
            // claim that outlives an unreachable node.
            let _ = ask_owner(&api, path).await;
            if other != 0 {
                eprintln!("create repo upstream: {other}"); // ponytail: eprintln
            }
            (StatusCode::BAD_GATEWAY, "could not create repository").into_response()
        }
    }
}

pub(crate) async fn list_repos(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(owner) = q.get("owner").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "owner is required").into_response();
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not list repositories").into_response();
        }
    }
    // `may_act_under` above established membership, so the private names under this owner are
    // this caller's to see — the same order `images` uses before it passes `true` on.
    match repo_listing(&api, owner, true).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list repos: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list repositories").into_response()
        }
    }
}

// ── repo settings ───────────────────────────────────────────────────────────
//
// Every route here answers the same two questions first: may this caller act in
// this namespace, and does this repo exist in it. The fleet is then asked to make
// the change, because the fleet is what enforces it — the directory's copy of
// visibility is for a badge in a list, and its copy of a protection rule would be
// a rule no push path can read.

/// The caller may act under `owner`, and `owner/name` is a repo there.
pub(crate) async fn settings_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
    name: &str,
) -> std::result::Result<&'a crate::directory::Directory, Response> {
    let user = caller(api, headers)?;
    let db = directory(api)?;
    if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid repository name").into_response());
    }
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::NOT_FOUND, "no such repository").into_response()),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return Err((StatusCode::BAD_GATEWAY, "could not read the repository").into_response());
        }
    }
    Ok(db)
}

#[derive(serde::Deserialize)]
pub(crate) struct RepoUpdate {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
}

pub(crate) async fn update_repo(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<RepoUpdate>,
) -> Response {
    // Only for the authorization it does: the change itself lands in the repo's own database on
    // the node that owns it.
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let public = match body.visibility.as_deref() {
        None => None,
        Some("public") => Some(true),
        Some("private") => Some(false),
        Some(_) => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };

    // The fleet first, and only then the index: the node's flag is what decides
    // who may read the repo, so a failure must leave the two agreeing on the OLD
    // answer rather than showing a public badge on a private repo.
    if let Some(p) = public {
        let vis = if p { "public" } else { "private" };
        let path = format!("/api/{}/{}/visibility?visibility={vis}", encode(&owner), encode(&name));
        match ask_owner(&api, path).await {
            Ok(200..=299) => {}
            Ok(404) => return (StatusCode::NOT_FOUND, "no such repository").into_response(),
            Ok(s) => {
                eprintln!("visibility upstream: {s}"); // ponytail: eprintln
                return (StatusCode::BAD_GATEWAY, "could not change visibility").into_response();
            }
            Err(r) => return r,
        }
    }
    // Same order, same reason: the repo's own database is the truth this is moving toward, so
    // it is written before the index row that mirrors it.
    if let Some(d) = body.description.as_deref() {
        let path = format!("/api/{}/{}/description?description={}", encode(&owner), encode(&name), encode(d));
        match ask_owner(&api, path).await {
            Ok(200..=299) => {}
            Ok(404) => return (StatusCode::NOT_FOUND, "no such repository").into_response(),
            Ok(s) => {
                eprintln!("description upstream: {s}"); // ponytail: eprintln
                return (StatusCode::BAD_GATEWAY, "could not save the change").into_response();
            }
            Err(r) => return r,
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Delete the repo, then forget it. That order is deliberate: the objects are the
/// thing worth removing, and an index row for a repo that is already gone is a
/// listing entry the next delete cleans up — where the reverse is a repo nobody
/// can see and everybody can still clone.
pub(crate) async fn delete_repo(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Only for the authorization it does: the change itself lands in the repo's own database on
    // the node that owns it.
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let path = format!("/api/{}/{}/delete", encode(&owner), encode(&name));
    match ask_owner(&api, path).await {
        Ok(200..=299) => {}
        Ok(s) => {
            eprintln!("delete upstream: {s}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not delete the repository").into_response();
        }
        Err(r) => return r,
    }
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) async fn list_protection(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let url = format!("{}/api/{}/{}/protect", api.upstream, encode(&owner), encode(&name));
    let r = match api.client.get(url).header(crate::proxy::PEER_HEADER, &api.secret).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("protection upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match read_bounded(r).await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => {
            eprintln!("protection body: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct ProtectionChange {
    pattern: String,
    #[serde(default)]
    remove: bool,
    #[serde(default = "yes")]
    no_force: bool,
    #[serde(default = "yes")]
    no_delete: bool,
}

pub(crate) fn yes() -> bool {
    true
}

pub(crate) async fn set_protection(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<ProtectionChange>,
) -> Response {
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }
    let pattern = body.pattern.trim();
    if pattern.is_empty() {
        return (StatusCode::BAD_REQUEST, "a branch pattern is required").into_response();
    }
    let mut path = format!(
        "/api/{}/{}/protect?pattern={}",
        encode(&owner),
        encode(&name),
        encode(pattern)
    );
    if body.remove {
        path.push_str("&remove=1");
    } else {
        if !body.no_force {
            path.push_str("&no_force=0");
        }
        if !body.no_delete {
            path.push_str("&no_delete=0");
        }
    }
    match ask_owner(&api, path).await {
        Ok(200..=299) => StatusCode::NO_CONTENT.into_response(),
        Ok(400) => (StatusCode::BAD_REQUEST, "that is not a branch pattern").into_response(),
        Ok(404) => (StatusCode::NOT_FOUND, "no such repository").into_response(),
        Ok(s) => {
            eprintln!("protect upstream: {s}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not save the rule").into_response()
        }
        Err(r) => r,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::*;

    /// The listing answers from markers alone — this suite has no Mongo fixture at all, so a
    /// marker with no row behind it listing correctly IS the cutover being proven.
    #[tokio::test]
    async fn a_repo_listing_reads_markers_not_mongo_rows() {
        let api = test_api_with_secret("s").await;
        crate::index::write(&api.store.os, crate::index::Kind::Repo, "alice", &test_marker("web", true))
            .await
            .unwrap();
        let out = repo_listing(&api, "alice", true).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "alice/web", "the `owner/name` identity the Mongo `_id` carried");
        assert_eq!(out[0].owner, "alice");
        assert_eq!(out[0].name, "web");
        assert!(out[0].public);
        assert_eq!(out[0].description, "the web repo");
        assert_eq!(out[0].created_by, "alice@example.com");
        assert_eq!(out[0].created_at, 1_700_000_000_000);
    }

    /// The leak test: a caller who is not a member gets `include_private = false`, and the
    /// private name must be absent from the SERIALIZED body, not merely from some filtered
    /// struct — the name itself is the thing that must not escape.
    #[tokio::test]
    async fn a_listing_without_private_access_never_names_a_private_repo() {
        let api = test_api_with_secret("s").await;
        for m in [test_marker("web", true), test_marker("skunkworks", false)] {
            crate::index::write(&api.store.os, crate::index::Kind::Repo, "alice", &m).await.unwrap();
        }
        let body = serde_json::to_string(&repo_listing(&api, "alice", false).await.unwrap()).unwrap();
        assert!(body.contains("web"), "the public repo is still listed");
        assert!(!body.contains("skunkworks"), "a private repo's NAME leaked into a public listing");

        let body = serde_json::to_string(&repo_listing(&api, "alice", true).await.unwrap()).unwrap();
        assert!(body.contains("skunkworks"), "a member sees both prefixes");
    }

    /// Both markers present is a crashed flip; it must read as private, in the listing too.
    #[tokio::test]
    async fn a_repo_with_both_markers_lists_as_private() {
        let api = test_api_with_secret("s").await;
        let m = test_marker("web", true);
        crate::index::put_in_place(&api.store.os, crate::index::Kind::Repo, "alice", &m).await.unwrap();
        crate::index::put_in_place(
            &api.store.os,
            crate::index::Kind::Repo,
            "alice",
            &crate::index::Marker { public: false, ..m },
        )
        .await
        .unwrap();
        assert!(repo_listing(&api, "alice", false).await.unwrap().is_empty(), "fail closed");
        let out = repo_listing(&api, "alice", true).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(!out[0].public);
    }
}
