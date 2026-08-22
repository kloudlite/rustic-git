use super::*;

// ── passkeys ────────────────────────────────────────────────────────────────
//
// WebAuthn is verified by the web app, which holds the relying-party identity and
// the challenge. This tier stores what verification needs — a public key and a
// counter — and answers the one question a sign-in asks before it knows who is
// signing in: whose credential is this?

use crate::directory::Passkey;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewPasskey {
    id: String,
    public_key: String,
    #[serde(default)]
    counter: i64,
    #[serde(default)]
    transports: Vec<String>,
    #[serde(default)]
    name: String,
}

pub(crate) async fn add_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewPasskey>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if body.id.trim().is_empty() || body.public_key.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a credential id and public key are required").into_response();
    }
    let name = match body.name.trim() {
        "" => "Passkey".to_string(),
        n => n.chars().take(60).collect(),
    };
    let key = Passkey {
        id: body.id.trim().to_string(),
        user: user.to_lowercase(),
        public_key: body.public_key.trim().to_string(),
        counter: body.counter,
        transports: body.transports,
        name,
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_passkey(&key).await {
        Ok(Some(())) => (StatusCode::CREATED, axum::Json(key)).into_response(),
        Ok(None) => (StatusCode::CONFLICT, "that passkey is already registered").into_response(),
        Err(e) => {
            eprintln!("add passkey: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not add the passkey").into_response()
        }
    }
}

pub(crate) async fn list_passkeys(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.passkeys_for(&user).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list passkeys: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list passkeys").into_response()
        }
    }
}

pub(crate) async fn remove_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Owned by the caller, or it does not exist as far as they are concerned.
    match db.passkey(&id).await {
        Ok(Some(p)) if p.user.eq_ignore_ascii_case(&user) => {}
        Ok(_) => return (StatusCode::NOT_FOUND, "no such passkey").into_response(),
        Err(e) => {
            eprintln!("passkey lookup: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not remove the passkey").into_response();
        }
    }
    match db.forget_passkey(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("remove passkey: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not remove the passkey").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct PasskeyLookup {
    id: String,
}

/// Whose passkey is this, and what verifies it?
///
/// PEER ONLY, and deliberately not reachable with a session: it is called during
/// sign-in, when there is no session yet. `caller` enforces that — with no Bearer
/// token it requires the peer secret, so the web app is the only thing that can
/// ask. A credential id is high-entropy and known only to the authenticator and
/// this server, but it still maps to an email, so it is not a public lookup.
pub(crate) async fn lookup_passkey(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<PasskeyLookup>,
) -> Response {
    if let Err(r) = caller(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.passkey(body.id.trim()).await {
        Ok(Some(p)) => axum::Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such passkey").into_response(),
        Err(e) => {
            eprintln!("passkey lookup: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not look up the passkey").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct PasskeyUsed {
    counter: i64,
}

/// Record the counter after a successful sign-in. Same peer-only reasoning as the
/// lookup: it happens before a session exists.
pub(crate) async fn passkey_used(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<PasskeyUsed>,
) -> Response {
    if let Err(r) = caller(&api, &headers) {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.advance_passkey(&id, body.counter).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("passkey counter: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not record the sign-in").into_response()
        }
    }
}

