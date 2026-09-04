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
            if e.downcast_ref::<rustic_git_pulls::directory::Invalid>().is_some() {
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
    user: rustic_git_pulls::directory::User,
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
            // Asked once, at the mint. The token is the only thing every later caller reads, so a
            // grant or a revocation takes effect on the next sign-in and nowhere else — which is
            // also its whole revocation story: the session's 12 h life is the window.
            let admin = db.is_superadmin(&u.email).await.unwrap_or(false);
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint_admin(&u.email, &u.name, u.username.as_deref(), admin) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::error!(error = %e, "minting token");
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: rustic_git_core::jwt::TTL_SECS }).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if e.downcast_ref::<rustic_git_pulls::directory::Invalid>().is_some() {
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
            let admin = db.is_superadmin(&u.email).await.unwrap_or(false);
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint_admin(&u.email, &u.name, u.username.as_deref(), admin) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::error!(error = %e, "minting token");
                        return (StatusCode::BAD_GATEWAY, "could not issue a token").into_response();
                    }
                },
                None => None,
            };
            axum::Json(SignIn { user: u, token, expires_in: rustic_git_core::jwt::TTL_SECS }).into_response()
        }
        Ok(None) => (StatusCode::CONFLICT, "that handle is taken").into_response(),
        Err(e) => {
            let msg = e.to_string();
            // Every rule in check_handle is the caller's to fix, and the message
            // says which rule — it is shown under the field.
            if e.downcast_ref::<rustic_git_pulls::directory::Invalid>().is_some() {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            tracing::error!(error = %msg, "claim username");
            (StatusCode::BAD_GATEWAY, "could not claim that handle").into_response()
        }
    }
}

// ── one team: read, settings, members, invitations ──────────────────────────
//
// The role model, whole, so nobody has to reassemble it from the checks below:
//
//   member  everything in the product — repos, images, workspaces, environments — and the
//           team's name and description. NOT inviting, NOT changing roles, NOT deleting.
//   admin   member, plus inviting and making other people admins.
//   owner   admin, plus making other people owners and deleting the team.
//
// Owners are additive: a team may have several, and an owner promotes another owner rather
// than handing over. The only rule that binds an owner is that the LAST one cannot step down
// or be removed — enforced in the directory, where every caller inherits it.
//
// Every route here authorizes on the members array of the team it names. The slug in the path
// says WHICH team; it never says whether the caller may touch it. A non-member gets 404, not 403,
// so the routes cannot be used to learn which slugs exist — the same shape the repo routes use.

use rustic_git_pulls::directory::{AcceptInvite, DeleteTeam, Invite, Membership, Role, Team};
use sha2::Digest;

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
    /// The public face, so the settings page renders its own current state without a second
    /// request. Any member may read it; only an admin may change it.
    public: bool,
    tagline: String,
    location: String,
    website: String,
    email: String,
    pins: Vec<String>,
    /// The caller's own role, so the page can decide which controls to render without a second
    /// request — and the server still refuses anything the role does not permit.
    your_role: Role,
    members: Vec<MemberDoc>,
    /// Open invitations. Only admins and owners see them — a member cannot invite, so has no
    /// business knowing who was asked.
    invites: Vec<InviteDoc>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InviteDoc {
    id: String,
    email: String,
    role: Role,
    invited_by: String,
    expires_at: String,
}

/// The team, with the caller's role in it — or the response that ends the request. `min` is the
/// least role that may proceed; `None` means any member.
async fn team_for<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    slug: &str,
    min: Option<Role>,
) -> std::result::Result<(String, Team, Role, &'a rustic_git_pulls::directory::Directory), Response> {
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
    let Some(role) = rustic_git_pulls::directory::Directory::role_of(&team, &user) else {
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
    let invites = if rank(role) >= rank(Role::Admin) {
        match db.invites_for(&slug).await {
            Ok(list) => list
                .into_iter()
                .map(|i| InviteDoc {
                    id: i.id,
                    email: i.email,
                    role: i.role,
                    invited_by: i.invited_by,
                    expires_at: i.expires_at.try_to_rfc3339_string().unwrap_or_default(),
                })
                .collect(),
            Err(e) => return db_err("read invitations", &slug, e),
        }
    } else {
        Vec::new()
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
        public: team.public,
        tagline: team.tagline,
        location: team.location,
        website: team.website,
        email: team.email,
        pins: team.pins,
        your_role: role,
        members,
        invites,
    })
    .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct TeamPatch {
    name: String,
    #[serde(default)]
    description: String,
    /// Governance, not work: only owners and admins may change what the public sees.
    #[serde(default)]
    profile: Option<ProfilePatch>,
}

/// Replace, not merge: every field defaults, so a `profile` block that omits one CLEARS it.
/// The settings form must always send the whole object as it should end up.
#[derive(serde::Deserialize)]
pub(crate) struct ProfilePatch {
    #[serde(default)]
    public: bool,
    #[serde(default)]
    tagline: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    website: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    pins: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileDoc {
    slug: String,
    name: String,
    description: String,
    tagline: String,
    location: String,
    website: String,
    email: String,
    member_count: usize,
    pins: Vec<String>,
    repos: Vec<PublicRepo>,
}

/// A repo as a STRANGER may see it. `RepoOut` carries `created_by` — a person's email address —
/// which has no business on an anonymous route.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicRepo {
    name: String,
    description: String,
    public: bool,
    created_at: i64,
}

/// Drop what a stranger may not see: private repos, and pins that name a private or deleted repo.
/// Pure so it has a test; `team_profile` applies it to a listing fetched with `include_private:
/// false` anyway — this is the belt to that route's braces.
pub(crate) fn public_face(
    repos: Vec<crate::repos::RepoOut>,
    pins: Vec<String>,
) -> (Vec<crate::repos::RepoOut>, Vec<String>) {
    let repos: Vec<_> = repos.into_iter().filter(|r| r.public).collect();
    let pins = pins.into_iter().filter(|p| repos.iter().any(|r| &r.name == p)).collect();
    (repos, pins)
}

/// `GET /v1/teams/{slug}/profile` — anonymous. 404 for a team that is not public, worded the same
/// as for a team that does not exist, so the route cannot be used to enumerate private teams.
pub(crate) async fn team_profile(
    State(api): State<Arc<Api>>,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response {
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let team = match db.get(&slug).await {
        Ok(Some(t)) if t.public => t,
        Ok(_) => return (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => return db_err("read team", &slug, e),
    };
    // `false`: this caller proved nothing.
    let repos = match crate::repos::repo_listing(&api, &slug, false).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(team = %slug, error = %e, "profile repos");
            return (StatusCode::BAD_GATEWAY, "could not read the team").into_response();
        }
    };
    let (repos, pins) = public_face(repos, team.pins);
    let repos = repos
        .into_iter()
        .map(|r| PublicRepo { name: r.name, description: r.description, public: r.public, created_at: r.created_at })
        .collect();
    axum::Json(ProfileDoc {
        slug: team.slug,
        name: team.name,
        description: team.description,
        tagline: team.tagline,
        location: team.location,
        website: team.website,
        email: team.email,
        member_count: team.members.len(),
        pins,
        repos,
    })
    .into_response()
}

pub(crate) async fn update_team(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
    axum::Json(body): axum::Json<TeamPatch>,
) -> Response {
    // Any member: the name and description are part of the work, not of governance.
    let (_, _, role, db) = match team_for(&api, &headers, &slug, None).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Before the write: a member who sends `profile` must change NOTHING, not just fail the
    // second half after the name has already moved.
    if body.profile.is_some() && rank(role) < rank(Role::Admin) {
        return (StatusCode::FORBIDDEN, "owner or admin only").into_response();
    }
    // Every check on the profile runs BEFORE the name write: a rejected pin must change nothing,
    // not leave the name moved and the profile as it was.
    let checked = match &body.profile {
        None => None,
        Some(p) => {
            // Pins are checked against the team's FULL listing — a member may pin a private repo,
            // and the profile route is what hides it from strangers.
            let names = match crate::repos::repo_listing(&api, &slug, true).await {
                Ok(r) => r.into_iter().map(|r| r.name).collect::<Vec<_>>(),
                Err(e) => {
                    tracing::error!(team = %slug, error = %e, "profile repos");
                    return (StatusCode::BAD_GATEWAY, "could not read the team").into_response();
                }
            };
            let pins = match rustic_git_pulls::directory::check_pins(&p.pins, &names) {
                Ok(v) => v,
                Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            };
            if let Err(msg) = check_website(&p.website) {
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            Some(pins)
        }
    };
    match db.update_team(&slug, &body.name, &body.description).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) if e.downcast_ref::<rustic_git_pulls::directory::Invalid>().is_some() => {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => return db_err("update team", &slug, e),
    }
    let (Some(p), Some(pins)) = (body.profile, checked) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let profile = rustic_git_pulls::directory::TeamProfile {
        public: p.public,
        tagline: p.tagline,
        location: p.location,
        website: p.website,
        email: p.email,
        pins,
    };
    match db.update_profile(&slug, &profile).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such team").into_response(),
        Err(e) => db_err("update team", &slug, e),
    }
}

/// The profile website goes onto a PUBLIC page as an `href`, so the scheme is decided here and
/// not left to whichever renderer's sanitiser happens to be in front of it: `javascript:`,
/// `data:` and friends are refused, not stored. Empty clears it.
fn check_website(w: &str) -> std::result::Result<(), &'static str> {
    if w.is_empty() {
        return Ok(());
    }
    if w.len() > 2048 {
        return Err("website is too long");
    }
    let rest = w
        .strip_prefix("https://")
        .or_else(|| w.strip_prefix("http://"))
        .ok_or("website must start with http:// or https://")?;
    // A host is what makes it a link; whitespace or a control character would let the value
    // reshape the attribute it lands in.
    if rest.is_empty() || w.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("website must be a valid http:// or https:// URL");
    }
    Ok(())
}

/// Who may grant which role. Written once here and read by both invite and set_role, so the
/// two cannot drift: an admin grants member or admin; an owner grants any.
fn may_grant(by: Role, role: Role) -> bool {
    match role {
        Role::Member | Role::Admin => rank(by) >= rank(Role::Admin),
        Role::Owner => by == Role::Owner,
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct NewInvite {
    email: String,
    #[serde(default = "member_role")]
    role: Role,
}

fn member_role() -> Role {
    Role::Member
}

/// What the caller gets back, ONCE: the raw token, which it puts in the email. Nothing here
/// stores it — the directory holds its hash.
#[derive(serde::Serialize)]
pub(crate) struct IssuedInvite {
    id: String,
    token: String,
    email: String,
    role: Role,
    /// So the mail can say which team, without the web app making a second request.
    team_name: String,
}

const INVITE_TTL_DAYS: i64 = 7;

fn invite_id(token: &str) -> String {
    rustic_git_core::hex(&sha2::Sha256::digest(token.as_bytes()))
}

pub(crate) async fn create_invite(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(slug): axum::extract::Path<String>,
    axum::Json(body): axum::Json<NewInvite>,
) -> Response {
    let (user, team, role, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !may_grant(role, body.role) {
        return (StatusCode::FORBIDDEN, "only an owner can invite an owner").into_response();
    }
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') {
        return (StatusCode::BAD_REQUEST, "a valid email is required").into_response();
    }
    if rustic_git_pulls::directory::Directory::role_of(&team, &email).is_some() {
        return (StatusCode::CONFLICT, "already a member").into_response();
    }
    // 32 random bytes, hex: unguessable, and URL-safe without encoding.
    let token = {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        rustic_git_core::hex(&b)
    };
    let now = mongodb::bson::DateTime::now();
    let invite = Invite {
        id: invite_id(&token),
        team: slug.clone(),
        email: email.clone(),
        role: body.role,
        invited_by: user,
        created_at: now,
        expires_at: mongodb::bson::DateTime::from_millis(now.timestamp_millis() + INVITE_TTL_DAYS * 86_400_000),
    };
    match db.create_invite(&invite).await {
        Ok(()) => (
            StatusCode::CREATED,
            axum::Json(IssuedInvite { id: invite.id, token, email, role: body.role, team_name: team.name }),
        )
            .into_response(),
        Err(e) => db_err("create invitation", &slug, e),
    }
}

pub(crate) async fn revoke_invite(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((slug, id)): axum::extract::Path<(String, String)>,
) -> Response {
    let (_, _, _, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.revoke_invite(&slug, &id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such invitation").into_response(),
        Err(e) => db_err("revoke invitation", &slug, e),
    }
}

/// What an invitation is for, shown on the accept page before the person commits. Needs a
/// session — a link alone reveals nothing — and answers 404 for a token that is spent,
/// expired or made up, all alike.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvitePreview {
    team: String,
    team_name: String,
    email: String,
    role: Role,
    invited_by: String,
}

pub(crate) async fn preview_invite(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    if caller(&api, &headers).is_err() {
        return rustic_git_core::httpx::unauthorized();
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let id = invite_id(&token);
    let inv = match db.invite(&id).await {
        Ok(Some(i)) => i,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such invitation").into_response(),
        Err(e) => return db_err("read invitation", &id, e),
    };
    let team_name = match db.get(&inv.team).await {
        Ok(Some(t)) => t.name,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such invitation").into_response(),
        Err(e) => return db_err("read invitation", &id, e),
    };
    axum::Json(InvitePreview {
        team: inv.team,
        team_name,
        email: inv.email,
        role: inv.role,
        invited_by: inv.invited_by,
    })
    .into_response()
}

pub(crate) async fn accept_invite(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let id = invite_id(&token);
    match db.accept_invite(&id, &user).await {
        Ok(AcceptInvite::Joined(team)) => axum::Json(serde_json::json!({ "team": team })).into_response(),
        // Signed in as someone else. Said plainly, because the fix is on their side: sign in
        // with the address the invitation was sent to.
        Ok(AcceptInvite::WrongEmail) => (
            StatusCode::FORBIDDEN,
            "this invitation was sent to a different email address",
        )
            .into_response(),
        Ok(AcceptInvite::NoSuchUser) => (StatusCode::CONFLICT, "sign in first").into_response(),
        Ok(AcceptInvite::Gone) => (StatusCode::NOT_FOUND, "no such invitation").into_response(),
        Err(e) => db_err("accept invitation", &id, e),
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
    let (_, team, role, db) = match team_for(&api, &headers, &slug, Some(Role::Admin)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let target = rustic_git_pulls::directory::Directory::role_of(&team, &email);
    // Granting the new role AND touching the old one both have to be within reach: an admin
    // may not demote an owner, however low the new role is.
    let allowed = may_grant(role, body.role) && target.is_none_or(|t| may_grant(role, t));
    if !allowed {
        return (StatusCode::FORBIDDEN, "your role does not allow that change").into_response();
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
    let target = rustic_git_pulls::directory::Directory::role_of(&team, &email);
    // Removing someone is the same reach as changing their role: an admin removes members and
    // admins, an owner removes anyone. Leaving is always yours to do.
    if !leaving && !target.is_some_and(|t| may_grant(role, t)) {
        return (StatusCode::FORBIDDEN, "your role does not allow that").into_response();
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

// ── magic sign-in links ──────────────────────────────────────────────────────
//
// Passwordless email sign-in. The web app asks for a link on someone's behalf, mails it, and
// redeems it when they click. Both calls are peer-only: no session exists yet on either side,
// and a Bearer path here would let a leaked token mint sign-in links for any address. The
// raw token is returned once and stored only as a hash.
//
// ponytail: no rate limit on minting. Cheap to abuse as a mail cannon against one address;
// add a per-email cooldown (one open link at a time) if it is ever pointed at someone.

const SIGNIN_TTL_MINUTES: i64 = 15;

#[derive(serde::Deserialize)]
pub(crate) struct SignInRequest {
    email: String,
}

#[derive(serde::Serialize)]
pub(crate) struct SignInLinkIssued {
    token: String,
    email: String,
}

pub(crate) async fn create_signin_link(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<SignInRequest>,
) -> Response {
    if let Err(r) = peer_only(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let email = body.email.trim().to_lowercase();
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return (StatusCode::BAD_REQUEST, "a valid email is required").into_response();
    }
    let token = {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        rustic_git_core::hex(&b)
    };
    let now = mongodb::bson::DateTime::now();
    let link = rustic_git_pulls::directory::SignInLink {
        id: invite_id(&token),
        email: email.clone(),
        created_at: now,
        expires_at: mongodb::bson::DateTime::from_millis(now.timestamp_millis() + SIGNIN_TTL_MINUTES * 60_000),
    };
    match db.create_signin(&link).await {
        Ok(()) => (StatusCode::CREATED, axum::Json(SignInLinkIssued { token, email })).into_response(),
        Err(e) => db_err("create sign-in link", &link.id, e),
    }
}

pub(crate) async fn redeem_signin_link(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    if let Err(r) = peer_only(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let id = invite_id(&token);
    match db.redeem_signin(&id).await {
        Ok(Some(email)) => axum::Json(serde_json::json!({ "email": email })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "that link is no longer valid").into_response(),
        Err(e) => db_err("redeem sign-in link", &id, e),
    }
}

// ── platform administrators ─────────────────────────────────────────────────

/// `POST`/`DELETE /api/admin/superadmins/{user}` — the list manages itself.
///
/// Superadmin-only, and it reads the CALLER's row rather than their token: this is the one surface
/// where a 12-hour-old claim is not good enough, because a revoked administrator holding a valid
/// token must not be able to grant themselves back.
async fn require_superadmin(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    let caller = caller(api, headers)?;
    let db = directory(api)?;
    match db.is_superadmin(&caller).await {
        Ok(true) => Ok(caller),
        Ok(false) => Err((StatusCode::FORBIDDEN, "admin only").into_response()),
        Err(e) => Err(db_err("check admin", &caller, e)),
    }
}

/// Same email either side of the `/`, case- and whitespace-insensitive: the directory lowercases
/// what it stores, but a caller's session email need not already be normalized.
fn is_same_user(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// "No one is left standing after this write" — the only shape that matters is the roster having
/// exactly one row and that row being the target, so this takes the roster rather than a count:
/// a count alone can't say WHICH one is left.
fn is_last_superadmin(admins: &[rustic_git_pulls::directory::SuperAdmin], target: &str) -> bool {
    matches!(admins, [only] if is_same_user(&only.user, target))
}

async fn write_audit(api: &Api, actor: &str, action: &'static str, target: &str, result: &'static str) {
    let entry = rustic_git_workspaces::audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        actor: actor.to_string(),
        action: action.to_string(),
        target: target.to_string(),
        // ponytail: no reason field on this route yet (add/remove predate the audit note
        // requirement) — carry `None` rather than block the roster on a body change out of scope
        // here; add a required `note` body alongside the other admin writes when this route
        // grows one.
        reason: None,
        result: result.into(),
    };
    if let Err(e) = rustic_git_workspaces::audit::record(&api.store.os, &entry).await {
        tracing::error!(error = %e, actor, action, target, "audit row not written");
    }
}

pub(crate) async fn add_superadmin(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(user): axum::extract::Path<String>,
) -> Response {
    let by = match require_superadmin(&api, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Minting a claim for an email with no account would be a superadmin nobody can sign in as.
    match db.user(&user).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::UNPROCESSABLE_ENTITY, "that email has no account").into_response(),
        Err(e) => return db_err("check account", &user, e),
    }
    match db.add_superadmin(&user, &by).await {
        Ok(()) => {
            write_audit(&api, &by, "add-superadmin", &user, "ok").await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => db_err("grant admin", &user, e),
    }
}

pub(crate) async fn remove_superadmin(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(user): axum::extract::Path<String>,
) -> Response {
    let by = match require_superadmin(&api, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if is_same_user(&by, &user) {
        return (StatusCode::CONFLICT, "you cannot remove your own administrator claim").into_response();
    }
    let admins = match db.superadmins().await {
        Ok(a) => a,
        Err(e) => return db_err("list admins", &user, e),
    };
    if is_last_superadmin(&admins, &user) {
        return (StatusCode::CONFLICT, "the last administrator cannot be removed").into_response();
    }
    match db.remove_superadmin(&user).await {
        Ok(()) => {
            write_audit(&api, &by, "remove-superadmin", &user, "ok").await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => db_err("revoke admin", &user, e),
    }
}

pub(crate) async fn list_superadmins(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_superadmin(&api, &headers).await {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.superadmins().await {
        Ok(rows) => axum::Json(rows).into_response(),
        Err(e) => db_err("list admins", "", e),
    }
}

#[cfg(test)]
mod role_tests {
    use super::{rank, Role};
    use rustic_git_pulls::directory::{Directory, Member, Team};
    use mongodb::bson::DateTime;

    fn team(members: &[(&str, Role)]) -> Team {
        Team {
            slug: "t".into(),
            name: "T".into(),
            created_by: "a@x".into(),
            created_at: DateTime::now(),
            members: members
                .iter()
                .map(|(u, r)| Member { user: (*u).into(), role: *r, joined_at: DateTime::now() })
                .collect(),
            ..Default::default()
        }
    }

    /// The whole role model, as a table. If this drifts from the comment at the top of the
    /// module, one of them is wrong — and this one is the one that runs.
    #[test]
    fn who_may_grant_what() {
        use super::may_grant;
        for r in [Role::Member, Role::Admin, Role::Owner] {
            assert!(!may_grant(Role::Member, r), "a member grants nothing");
        }
        assert!(may_grant(Role::Admin, Role::Member));
        assert!(may_grant(Role::Admin, Role::Admin));
        assert!(!may_grant(Role::Admin, Role::Owner), "only an owner makes an owner");
        for r in [Role::Member, Role::Admin, Role::Owner] {
            assert!(may_grant(Role::Owner, r), "an owner grants anything");
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

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn a_profile_names_only_public_repos_and_only_live_pins() {
        let repos = vec![
            crate::repos::RepoOut { id: "acme/web".into(), owner: "acme".into(), name: "web".into(), public: true, description: String::new(), created_by: String::new(), created_at: 0 },
            crate::repos::RepoOut { id: "acme/secret".into(), owner: "acme".into(), name: "secret".into(), public: false, description: String::new(), created_by: String::new(), created_at: 0 },
        ];
        let pins = vec!["secret".to_string(), "web".to_string(), "gone".to_string()];
        let (repos, pins) = public_face(repos, pins);
        assert_eq!(repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["web"]);
        assert_eq!(pins, vec!["web".to_string()], "a private or deleted pin is not shown");
    }

    /// The rejects are what `update_team` turns into a 400; the accepts are stored verbatim.
    #[test]
    fn website_is_http_or_https_or_nothing() {
        for ok in ["", "https://example.com", "http://example.com/a?b=c#d"] {
            assert!(check_website(ok).is_ok(), "{ok:?} should be accepted");
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,hi",
            "vbscript:x",
            "file:///etc/passwd",
            "example.com",
            "https://",
            "https://ex ample.com",
            "https://example.com\n",
        ] {
            assert!(check_website(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(check_website(&format!("https://{}", "a".repeat(2048))).is_err());
    }
}

// The three refusal rules on `add_superadmin`/`remove_superadmin` are tested as the pure
// decisions they reduce to (`is_same_user`, `is_last_superadmin`) rather than through the
// handlers: `Directory` is a concrete mongo-backed struct with no double, and there is no mongo
// test harness in this workspace (`crates/pulls`' own directory tests are the same shape — see
// `role_lookup_ignores_email_case` above). The `db.user`/`db.add_superadmin` calls those rules
// gate are exercised by the e2e suite against a real cluster instead.
#[cfg(test)]
mod superadmin_rule_tests {
    use super::{is_last_superadmin, is_same_user};
    use mongodb::bson::DateTime;
    use rustic_git_pulls::directory::SuperAdmin;

    fn admin(user: &str) -> SuperAdmin {
        SuperAdmin { user: user.into(), added_at: DateTime::now(), added_by: "bootstrap".into() }
    }

    /// A superadmin cannot remove their own claim through this route — the spec's exact wording.
    #[test]
    fn remove_superadmin_refuses_to_remove_yourself() {
        assert!(is_same_user("a@x", "a@x"));
        assert!(is_same_user(" A@X ", "a@x"), "case and whitespace must not defeat the check");
        assert!(!is_same_user("a@x", "b@x"));
    }

    /// The last superadmin cannot be removed by anyone, even another superadmin.
    #[test]
    fn remove_superadmin_refuses_to_remove_the_last_one() {
        let one = [admin("a@x")];
        assert!(is_last_superadmin(&one, "a@x"));
        let two = [admin("a@x"), admin("b@x")];
        assert!(!is_last_superadmin(&two, "a@x"), "a second row means removal is still safe");
    }
}
