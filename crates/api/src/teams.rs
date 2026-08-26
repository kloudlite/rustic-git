use super::*;

// ── teams ───────────────────────────────────────────────────────────────────
//
// Callers are trusted infrastructure, not browsers: the web app holds the peer
// secret and states which signed-in user it is acting for. The end user's
// identity is never taken from anything the browser can set.

#[derive(serde::Deserialize)]
pub(crate) struct NewTeam {
    slug: String,
    name: String,
}


pub(crate) async fn create_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewTeam>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match db.create(body.slug.trim(), &body.name, &user).await {
        Ok(Some(team)) => (StatusCode::CREATED, axum::Json(team)).into_response(),
        // Taken, not an error: the caller shows "that handle is in use" and the
        // form stays on screen.
        Ok(None) => (StatusCode::CONFLICT, "handle already taken").into_response(),
        Err(e) => {
            let msg = e.to_string();
            // A rejected handle is the caller's mistake; anything else is ours and
            // must not echo the database's words back to a user.
            if msg.contains("invalid team handle") || msg.contains("team name required") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            tracing::error!(error = %msg, "create team");
            (StatusCode::BAD_GATEWAY, "could not create team").into_response()
        }
    }
}

pub(crate) async fn list_teams(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match db.for_user(&user).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            tracing::error!(user = %user, error = %e, "list teams");
            (StatusCode::BAD_GATEWAY, "could not list teams").into_response()
        }
    }
}

/// What sign-in answers with: who they are, and the token to present next time.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignIn {
    user: crate::directory::User,
    /// `None` when the server has no signing key: the user still exists, but the
    /// caller must keep using the peer path rather than silently treating an
    /// absent token as a valid one.
    token: Option<String>,
    expires_in: u64,
}

#[derive(serde::Deserialize)]
pub(crate) struct NewUser {
    email: String,
    #[serde(default)]
    name: String,
}

pub(crate) async fn upsert_user(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewUser>,
) -> Response {
    // Peer only: this route MINTS a session, so a session must not be able to call it — a leaked
    // token would otherwise renew itself for as long as the holder likes. The peer's assertion of
    // who signed in must still agree with the body, or a caller holding the secret could mint any
    // identity it likes.
    let asserted = match peer_only(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if asserted.to_lowercase() != body.email.trim().to_lowercase() {
        return (StatusCode::BAD_REQUEST, "caller identity does not match the body").into_response();
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.upsert_user(&body.email, &body.name).await {
        Ok(u) => {
            // The token is minted here and nowhere else, so the signing key lives
            // in one process. The web app receives it and presents it on every
            // later call rather than re-asserting who the user is.
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint(&u.email, &u.name, u.username.as_deref()) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::error!(error = %e, "minting token");
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: crate::jwt::TTL_SECS }).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("valid email") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            tracing::error!(error = %msg, "upsert user");
            (StatusCode::BAD_GATEWAY, "could not record user").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct NewUsername {
    username: String,
}

pub(crate) async fn claim_username(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewUsername>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.claim_username(&user, &body.username).await {
        Ok(Some(u)) => {
            // A new token: the old one says they have no handle, and every caller
            // reads that claim rather than asking again.
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint(&u.email, &u.name, u.username.as_deref()) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::error!(error = %e, "minting token");
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: crate::jwt::TTL_SECS }).into_response()
        }
        Ok(None) => (StatusCode::CONFLICT, "that handle is taken").into_response(),
        Err(e) => {
            let msg = e.to_string();
            // Every rule in check_handle is the caller's to fix, and the message
            // says which rule — it is shown under the field.
            if msg.contains("handle") || msg.contains("username already set") || msg.contains("no such user") {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            tracing::error!(error = %msg, "claim username");
            (StatusCode::BAD_GATEWAY, "could not claim that handle").into_response()
        }
    }
}

// ── one team: read, settings, members, ownership ────────────────────────────
//
// Every route here authorizes on the members array of the team it names. The slug in the path
// says WHICH team; it never says whether the caller may touch it. A non-member gets 404, not 403,
// so the routes cannot be used to learn which slugs exist — the same shape the repo routes use.

use crate::directory::{AddMember, DeleteTeam, Membership, Role, Team};

/// A member as the page shows them: the directory row joined onto the membership entry.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemberDoc {
    email: String,
    name: String,
    /// Absent for someone who signed in but never picked a handle. They can still hold a role.
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    role: Role,
    joined_at: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamDoc {
    slug: String,
    name: String,
    description: String,
    created_at: String,
    /// The caller's own role, so the page can decide which controls to render without a second
    /// request — and the server still refuses anything the role does not permit.
    your_role: Role,
    members: Vec<MemberDoc>,
}

/// The team, with the caller's role in it — or the response that ends the request. `min` is the
/// least role that may proceed; `None` means any member.
async fn team_for<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    slug: &str,
    min: Option<Role>,
) -> std::result::Result<(String, Team, Role, &'a crate::directory::Directory), Response> {
    let user = caller(api, headers)?;
    let db = directory(api)?;
    let team = match db.get(slug).await {
        Ok(Some(t)) => t,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such team").into_response()),
        Err(e) => {
            tracing::error!(team = %slug, error = %e, "read team");
            return Err((StatusCode::BAD_GATEWAY, "could not read team").into_response());
        }
    };
    let Some(role) = crate::directory::Directory::role_of(&team, &user) else {
        return Err((StatusCode::NOT_FOUND, "no such team").into_response());
    };
    if let Some(min) = min {
        if rank(role) < rank(min) {
            return Err((StatusCode::FORBIDDEN, "your role does not allow that").into_response());
        }
    }
    Ok((user, team, role, db))
}

/// Owner > Admin > Member. The enum's declaration order says the same thing, but a comparison
/// that depends on declaration order is a comparison that silently breaks on a reorder.
fn rank(r: Role) -> u8 {
    match r {
        Role::Owner => 2,
        Role::Admin => 1,
        Role::Member => 0,
    }
}

fn db_err(what: &str, slug: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(team = %slug, error = %e, "{what}");
    (StatusCode::BAD_GATEWAY, format!("could not {what}")).into_response()
}

pub(crate) async fn get_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response {
    let (_, _, role, db) = match team_for(&api, &headers, &slug, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let (team, users) = match db.describe(&slug).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => return db_err("read team", &slug, e),
    };
    let members = team
        .members
        .iter()
        .map(|m| {
            let u = users.iter().find(|u| u.email.eq_ignore_ascii_case(&m.user));
            MemberDoc {
                email: m.user.clone(),
                // Email as the name for a row the directory no longer has: the role still exists
                // and the page must show who holds it.
                name: u.map(|u| u.name.clone()).unwrap_or_else(|| m.user.clone()),
                username: u.and_then(|u| u.username.clone()),
                role: m.role,
                joined_at: m.joined_at.try_to_rfc3339_string().unwrap_or_default(),
            }
        })
        .collect();
    axum::Json(TeamDoc {
        slug: team.slug,
        name: team.name,
        description: team.description,
        created_at: team.created_at.try_to_rfc3339_string().unwrap_or_default(),
        your_role: role,
        members,
    })
    .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct TeamPatch {
    name: String,
    #[serde(default)]
    description: String,
}

pub(crate) async fn update_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
    axum::Json(body): axum::Json<TeamPatch>,
) -> Response {
    let (_, _, _, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.update_team(&slug, &body.name, &body.description).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) if e.to_string().contains("team name required") => {
            (StatusCode::BAD_REQUEST, "team name required").into_response()
        }
        Err(e) => db_err("update team", &slug, e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct NewMember {
    email: String,
    #[serde(default = "member_role")]
    role: Role,
}

fn member_role() -> Role {
    Role::Member
}

pub(crate) async fn add_member(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
    axum::Json(body): axum::Json<NewMember>,
) -> Response {
    let (_, _, role, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Only an owner can create another owner. Adding someone AS owner is ownership transfer by
    // another door, and that door is `transfer`, which demotes the giver in the same write.
    if body.role == Role::Owner {
        return (StatusCode::BAD_REQUEST, "use transfer to make someone an owner").into_response();
    }
    if body.role == Role::Admin && role != Role::Owner {
        return (StatusCode::FORBIDDEN, "only an owner can add an admin").into_response();
    }
    match db.add_member(&slug, &body.email, body.role).await {
        Ok(AddMember::Added) => StatusCode::CREATED.into_response(),
        Ok(AddMember::AlreadyMember) => (StatusCode::CONFLICT, "already a member").into_response(),
        // 404 on the PERSON, worded so the page can say what to do about it: there is no
        // invitation, so someone who has never signed in cannot be added yet.
        Ok(AddMember::NoSuchUser) => {
            (StatusCode::NOT_FOUND, "no account with that email has signed in yet").into_response()
        }
        Ok(AddMember::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("add member", &slug, e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct RolePatch {
    role: Role,
}

pub(crate) async fn set_role(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((slug, email)): axum::extract::Path<(String, String)>,
    axum::Json(body): axum::Json<RolePatch>,
) -> Response {
    let (user, team, role, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if body.role == Role::Owner {
        return (StatusCode::BAD_REQUEST, "use transfer to make someone an owner").into_response();
    }
    // An admin may move people between member and admin, but may not touch an owner — that is
    // demotion of a superior, and only another owner can do it.
    let target = crate::directory::Directory::role_of(&team, &email);
    if role != Role::Owner && (target == Some(Role::Owner) || body.role == Role::Admin) {
        return (StatusCode::FORBIDDEN, "only an owner can change that role").into_response();
    }
    if user.eq_ignore_ascii_case(&email) && role == Role::Owner {
        return (StatusCode::BAD_REQUEST, "transfer ownership before stepping down").into_response();
    }
    match db.set_role(&slug, &email, body.role).await {
        Ok(Membership::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(Membership::NotAMember) => (StatusCode::NOT_FOUND, "not a member").into_response(),
        Ok(Membership::LastOwner) => {
            (StatusCode::CONFLICT, "a team must keep at least one owner").into_response()
        }
        Ok(Membership::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("change role", &slug, e),
    }
}

pub(crate) async fn remove_member(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((slug, email)): axum::extract::Path<(String, String)>,
) -> Response {
    // Any member may remove THEMSELVES; removing someone else takes admin, and removing an
    // owner takes an owner.
    let (user, team, role, db) = match team_for(&api, &headers, &slug, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let leaving = user.eq_ignore_ascii_case(&email);
    let target = crate::directory::Directory::role_of(&team, &email);
    if !leaving {
        if rank(role) < rank(Role::Admin) {
            return (StatusCode::FORBIDDEN, "your role does not allow that").into_response();
        }
        if target == Some(Role::Owner) && role != Role::Owner {
            return (StatusCode::FORBIDDEN, "only an owner can remove an owner").into_response();
        }
    }
    match db.remove_member(&slug, &email).await {
        Ok(Membership::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(Membership::NotAMember) => (StatusCode::NOT_FOUND, "not a member").into_response(),
        Ok(Membership::LastOwner) => (
            StatusCode::CONFLICT,
            if leaving {
                "transfer ownership before leaving"
            } else {
                "a team must keep at least one owner"
            },
        )
            .into_response(),
        Ok(Membership::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("remove member", &slug, e),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct Transfer {
    to: String,
}

pub(crate) async fn transfer_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
    axum::Json(body): axum::Json<Transfer>,
) -> Response {
    let (user, _, _, db) = match team_for(&api, &headers, &slug, Some(Role::Owner)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.transfer(&slug, &user, &body.to).await {
        Ok(Membership::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(Membership::NotAMember) => {
            (StatusCode::NOT_FOUND, "that person is not a member of this team").into_response()
        }
        Ok(Membership::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Ok(Membership::LastOwner) => unreachable!("transfer never removes an owner"),
        Err(e) => db_err("transfer team", &slug, e),
    }
}

pub(crate) async fn delete_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response {
    let (user, _, _, db) = match team_for(&api, &headers, &slug, Some(Role::Owner)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.delete_team(&slug).await {
        Ok(DeleteTeam::Deleted) => {
            tracing::info!(team = %slug, by = %user, "team deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        // Worded for the person: what is in the way, and what to do about it.
        Ok(DeleteTeam::StillOwns { repos }) => (
            StatusCode::CONFLICT,
            format!(
                "{slug} still owns {repos} {}; delete or move them first",
                if repos == 1 { "repository" } else { "repositories" }
            ),
        )
            .into_response(),
        Ok(DeleteTeam::NoSuchTeam) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("delete team", &slug, e),
    }
}

#[cfg(test)]
mod role_tests {
    use super::{rank, Role};
    use crate::directory::{Directory, Member, Team};
    use mongodb::bson::DateTime;

    fn team(members: &[(&str, Role)]) -> Team {
        Team {
            slug: "t".into(),
            name: "T".into(),
            description: String::new(),
            created_by: "a@x".into(),
            created_at: DateTime::now(),
            members: members
                .iter()
                .map(|(u, r)| Member { user: (*u).into(), role: *r, joined_at: DateTime::now() })
                .collect(),
        }
    }

    /// Every authorization decision above is `rank(role) < rank(min)`. If the order ever
    /// drifts, an admin outranks an owner and the danger zone opens to the wrong people.
    #[test]
    fn owner_outranks_admin_outranks_member() {
        assert!(rank(Role::Owner) > rank(Role::Admin));
        assert!(rank(Role::Admin) > rank(Role::Member));
    }

    /// Emails arrive in whatever case the identity provider and the browser chose; the
    /// membership row was lowercased at write. A case-sensitive lookup here would lock a real
    /// owner out of their own team on the basis of a capital letter.
    #[test]
    fn role_lookup_ignores_email_case() {
        let t = team(&[("Owner@Example.com", Role::Owner), ("m@example.com", Role::Member)]);
        assert_eq!(Directory::role_of(&t, "owner@example.com"), Some(Role::Owner));
        assert_eq!(Directory::role_of(&t, "M@EXAMPLE.COM"), Some(Role::Member));
        assert_eq!(Directory::role_of(&t, "nobody@example.com"), None);
    }
}
