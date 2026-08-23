use super::*;

// ── credentials ─────────────────────────────────────────────────────────────
//
// A credential acts in exactly ONE namespace, chosen when it is made, because
// that is what the git fleet enforces: `auth::authorize` compares the credential's
// owner to the repo's owner, with no membership lookup — the nodes have no
// directory. Scoping here to a namespace the caller belongs to keeps the two ends
// saying the same thing, and means a leaked laptop key cannot reach a team's repos
// unless it was made for them.

use crate::directory::{Credential, CredentialKind};

#[derive(serde::Deserialize)]
pub(crate) struct NewCredential {
    owner: String,
    #[serde(default)]
    name: String,
    /// ssh keys only: the OpenSSH public key line.
    #[serde(default)]
    key: String,
    /// Register this key for SIGNING rather than for access. The same key may be
    /// added both ways; they are separate entries because they grant separate
    /// things.
    #[serde(default)]
    signing: bool,
}

/// A token, the one time it is readable. Everything else about it can be looked up
/// forever; the secret cannot, because only its digest is kept.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IssuedToken {
    token: String,
    #[serde(flatten)]
    meta: Credential,
}

/// The caller, and their right to act in `owner`. Every credential route starts
/// here, so none of them can be reached for a namespace that is not the caller's.
pub(crate) async fn credential_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
) -> std::result::Result<(String, &'a crate::directory::Directory), Response> {
    let user = caller(api, headers)?;
    let db = directory(api)?;
    match may_act_under(db, &user, owner).await {
        Ok(true) => Ok((user, db)),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such owner").into_response()),
        Err(e) => {
            eprintln!("credential authorization: {e}"); // ponytail: eprintln
            Err((StatusCode::BAD_GATEWAY, "could not read credentials").into_response())
        }
    }
}

/// `?owner=` for the list routes.
pub(crate) fn owner_param(q: &std::collections::HashMap<String, String>) -> std::result::Result<String, Response> {
    q.get("owner")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "owner is required").into_response())
}

pub(crate) async fn create_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "give the token a name").into_response();
    }
    if name.chars().count() > 60 {
        return (StatusCode::BAD_REQUEST, "that name is too long").into_response();
    }

    // The secret is created FIRST and the index second, so a crash between them
    // leaves a working token nobody can see rather than a listed token that does
    // not work. The unwind below closes that window in the ordinary case.
    let token = match api.store.create_token(&owner).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("create token: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    };
    let meta = Credential {
        id: crate::store::Store::token_digest(&token),
        kind: CredentialKind::Token,
        owner: owner.clone(),
        created_by: user,
        name: name.to_string(),
        material: String::new(),
        fingerprints: Vec::new(),
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        // A digest collision is not a thing that happens; treat it as our failure.
        Ok(None) | Err(_) => {
            if let Err(e) = api.store.revoke_token_digest(&meta.id).await {
                eprintln!("unwinding token: {e}"); // ponytail: eprintln
            }
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    }
    // The only time the token is ever readable.
    (StatusCode::CREATED, axum::Json(IssuedToken { token, meta })).into_response()
}

pub(crate) async fn list_tokens(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.credentials_for(&owner, CredentialKind::Token).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list tokens: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list tokens").into_response()
        }
    }
}

/// Revoke by id. The index is deleted LAST: if the object delete fails the
/// credential stays listed and revocable, which is the safe direction — a listed
/// token that still works can be revoked again, an unlisted one that still works
/// cannot be revoked at all.
pub(crate) async fn revoke_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::Token).await
}

pub(crate) async fn remove_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::SshKey).await
}

pub(crate) async fn revoke(
    api: Arc<Api>,
    headers: axum::http::HeaderMap,
    id: String,
    kind: CredentialKind,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let found = match db.credential(&id).await {
        Ok(Some(c)) if c.kind == kind => c,
        // A credential of the wrong kind is reported as missing rather than as a
        // mistake: the id space is shared, and saying "that is an ssh key" tells a
        // caller something about a credential that may not be theirs.
        Ok(_) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            eprintln!("revoke lookup: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    };
    // Authorized against the credential's OWNER, never against holding its id.
    match may_act_under(db, &user, &found.owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            eprintln!("revoke authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    }
    let gone = match kind {
        CredentialKind::Token => api.store.revoke_token_digest(&id).await,
        CredentialKind::SshKey => api.store.remove_ssh_key(&id).await,
        // A signing key never authenticates anything, so it was never written to
        // the store the fleet reads. Forgetting the row is the whole of it.
        CredentialKind::SigningKey => Ok(()),
    };
    if let Err(e) = gone {
        eprintln!("revoke: {e}"); // ponytail: eprintln
        return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
    }
    if let Err(e) = db.forget_credential(&id).await {
        // The credential no longer works, which is what was asked for. It will
        // linger in the list until the next attempt succeeds.
        eprintln!("forget credential {id}: {e}"); // ponytail: eprintln
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The credential id and the fingerprints an ssh SIGNING key answers to. Kept beside `add_key`
/// and used by it, so a test can build exactly the row registration writes.
pub(crate) fn ssh_signing_fingerprints(key_line: &str) -> crate::Result<(String, Vec<String>)> {
    let f = crate::store::Store::ssh_fingerprint(key_line)?;
    // Lowercased: `signer_by_any` lowercases what a signature presents and Mongo's `$in` is an
    // exact match, while `SHA256:<base64>` is mixed case. Stored as-is, no ssh signature ever
    // found its key. The id keeps the original spelling — it is only ever matched by itself.
    Ok((f.clone(), vec![f.to_lowercase()]))
}

pub(crate) async fn add_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // An armoured OpenPGP block is a signing key and nothing else — it cannot
    // authenticate an ssh connection, so it is only accepted for signing.
    let is_gpg = body.key.contains("BEGIN PGP PUBLIC KEY BLOCK");
    if is_gpg && !body.signing {
        return (
            StatusCode::BAD_REQUEST,
            "a GPG key can only be added as a signing key",
        )
            .into_response();
    }

    // Parsed before anything is written, so a malformed key is a 400 rather than a
    // row describing a key the fleet never accepted.
    let (fingerprint, fingerprints) = if is_gpg {
        match crate::gpg::fingerprints_of(&body.key) {
            // The primary key names the credential; every subkey is indexed, so a
            // signature made by one finds its owner without a scan.
            Ok(all) if !all.is_empty() => (all[0].clone(), all),
            _ => return (StatusCode::BAD_REQUEST, "that is not an OpenPGP public key").into_response(),
        }
    } else {
        match ssh_signing_fingerprints(&body.key) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
    // The comment at the end of the key line, when they did not name it — which is
    // usually `user@machine` and is exactly what they would have typed.
    let name = match body.name.trim() {
        "" if is_gpg => crate::gpg::emails_of(&body.key)
            .ok()
            .and_then(|e| e.first().cloned())
            .unwrap_or_else(|| "GPG key".to_string()),
        "" => body.key.split_whitespace().nth(2).unwrap_or("ssh key").to_string(),
        n => n.to_string(),
    };

    let meta = Credential {
        // Prefixed, so one key registered for both purposes is two rows.
        id: if body.signing { format!("sign:{fingerprint}") } else { fingerprint.clone() },
        kind: if body.signing { CredentialKind::SigningKey } else { CredentialKind::SshKey },
        owner: owner.clone(),
        created_by: user,
        name,
        // Only a GPG key keeps its material: an ssh signature carries its own.
        material: if is_gpg { body.key.clone() } else { String::new() },
        fingerprints,
        created_at: mongodb::bson::DateTime::now(),
    };
    // Index first here, unlike a token: the id is the key's own fingerprint rather
    // than a fresh secret, so the insert is what makes "already added" detectable.
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        Ok(None) => return (StatusCode::CONFLICT, "that key is already added").into_response(),
        Err(e) => {
            eprintln!("add key: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
    }
    // Only an ACCESS key goes to the store the git nodes authenticate against. A
    // signing key there would silently grant push rights to anyone who added a key
    // to prove authorship.
    if !body.signing && !is_gpg {
        if let Err(e) = api.store.add_ssh_key(&owner, &body.key).await {
            let _ = db.forget_credential(&meta.id).await;
            eprintln!("add key: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
    }
    (StatusCode::CREATED, axum::Json(meta)).into_response()
}

pub(crate) async fn list_keys(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kind = match q.get("kind").map(String::as_str) {
        Some("signing") => CredentialKind::SigningKey,
        _ => CredentialKind::SshKey,
    };
    match db.credentials_for(&owner, kind).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            eprintln!("list keys: {e}"); // ponytail: eprintln
            (StatusCode::BAD_GATEWAY, "could not list keys").into_response()
        }
    }
}

