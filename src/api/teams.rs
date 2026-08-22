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
            eprintln!("create team: {msg}"); // ponytail: eprintln
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
            eprintln!("list teams: {e}"); // ponytail: eprintln
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
    // The caller header is the peer's assertion of who signed in; the body must
    // agree with it. Taking the email from the body alone would let a caller that
    // holds the peer secret mint any identity it likes.
    let asserted = match caller(&api, &headers) {
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
                        eprintln!("minting token: {e}"); // ponytail: eprintln
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
            eprintln!("upsert user: {msg}"); // ponytail: eprintln
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
                        eprintln!("minting token: {e}"); // ponytail: eprintln
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
            eprintln!("claim username: {msg}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not claim that handle").into_response()
        }
    }
}
