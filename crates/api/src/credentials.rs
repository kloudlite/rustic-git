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
            tracing::error!(owner = %owner, error = %e, "credential authorization");
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
            tracing::error!(owner = %owner, error = %e, "create token");
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
                tracing::warn!(error = %e, "unwinding token");
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
            tracing::error!(owner = %owner, error = %e, "list tokens");
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
            tracing::error!(error = %e, "revoke lookup");
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    };
    // Authorized against the credential's OWNER, never against holding its id.
    match may_act_under(db, &user, &found.owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "revoke authorization");
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
        tracing::error!(error = %e, "revoke");
        return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
    }
    if let Err(e) = db.forget_credential(&id).await {
        // The credential no longer works, which is what was asked for. It will
        // linger in the list until the next attempt succeeds.
        tracing::warn!(credential = %id, error = %e, "forget credential");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it.
/// The production `ssh_fingerprint` (a test-only twin lives in `tests/common`): the only consumer in
/// this crate, and duplicating eight lines is cheaper than adding a shared axum-free home for a
/// function that needs `russh` only here.
fn ssh_fingerprint(line: &str) -> crate::Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

/// The credential id and the fingerprints an ssh SIGNING key answers to. Kept beside `add_key`
/// and used by it, so a test can build exactly the row registration writes.
pub(crate) fn ssh_signing_fingerprints(key_line: &str) -> crate::Result<(String, Vec<String>)> {
    let f = ssh_fingerprint(key_line)?;
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
            tracing::error!(owner = %owner, error = %e, "add key");
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
    }
    // Only an ACCESS key goes to the store the git nodes authenticate against. A
    // signing key there would silently grant push rights to anyone who added a key
    // to prove authorship.
    if !body.signing && !is_gpg {
        if let Err(e) = api.store.add_ssh_key(&owner, &fingerprint).await {
            let _ = db.forget_credential(&meta.id).await;
            tracing::error!(owner = %owner, error = %e, "add key");
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
            tracing::error!(owner = %owner, error = %e, "list keys");
            (StatusCode::BAD_GATEWAY, "could not list keys").into_response()
        }
    }
}


// ── the platform-issued key ──────────────────────────────────────────────
//
// One keypair per user, generated by us rather than supplied by them. The private half is written
// into every workspace so `git push` from inside one works without anybody pasting a credential;
// the public half is registered exactly like a user-added key, so the auth path is unchanged.
//
// The blast radius is real and worth stating where someone will read it: the key is the user's git
// identity, and it sits in every workspace they own, readable by anything running there — a
// malicious postinstall included. Regenerating is the remedy, which is why revocation is part of
// rotation rather than a separate chore.
// ponytail: one key for every workspace; a key per workspace would confine a compromise to one,
// at the cost of a fingerprint per workspace to list and revoke.

/// `ssh-keygen`, not a Rust keygen crate — the same choice `boot.rs` makes for the host key, for
/// the same reason: it avoids a second `rand_core` in the graph, and every image here already
/// carries `openssh-client`.
fn generate_ed25519() -> std::io::Result<(String, String)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("id_ed25519");
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "rustic-git", "-f"])
        .arg(&path)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    let private = std::fs::read_to_string(&path)?;
    let public = std::fs::read_to_string(path.with_extension("pub"))?;
    Ok((private, public.trim().to_string()))
}

#[derive(serde::Serialize)]
pub(crate) struct PlatformKey {
    pub public: String,
    pub fingerprint: String,
}

/// The user's platform key, generating one on first read.
///
/// Lazy rather than hooked into user creation: an account that never opens a workspace never needs
/// one, and "generate on signup" is a migration for every account that already exists.
pub(crate) async fn platform_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = credential_caller(&api, &headers, &owner).await {
        return r;
    }
    match ensure_platform_key(&api, &owner, false).await {
        Ok(k) => axum::Json(k).into_response(),
        Err(r) => r,
    }
}

/// Replace the key, revoking the old one.
pub(crate) async fn regenerate_platform_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = credential_caller(&api, &headers, &owner).await {
        return r;
    }
    match ensure_platform_key(&api, &owner, true).await {
        Ok(k) => axum::Json(k).into_response(),
        Err(r) => r,
    }
}

async fn ensure_platform_key(api: &Api, owner: &str, force: bool) -> std::result::Result<PlatformKey, Response> {
    let bad = |what: &str| (StatusCode::BAD_GATEWAY, what.to_string()).into_response();

    let existing = api.store.user_key(owner).await.map_err(|_| bad("could not read the key"))?;
    let old_fp = existing.as_deref().and_then(|p| fingerprint_of_private(p).ok());

    if !force {
        if let Some(private) = &existing {
            let (public, fingerprint) =
                public_of_private(private).map_err(|_| bad("stored key is unreadable"))?;
            return Ok(PlatformKey { public, fingerprint });
        }
    }

    let (private, public) = generate_ed25519().map_err(|_| bad("could not generate a key"))?;
    // The same fingerprint the auth path indexes by, so a generated key is looked up exactly
    // like a user-added one.
    let fingerprint = ssh_fingerprint(&public).map_err(|_| bad("generated key is unreadable"))?;
    api.store
        .rotate_user_key(owner, &private, &fingerprint, old_fp.as_deref())
        .await
        .map_err(|_| bad("could not install the key"))?;
    tracing::info!(%owner, replaced = old_fp.is_some(), "installed a platform key");
    Ok(PlatformKey { public, fingerprint })
}

/// The OpenSSH public line and fingerprint for a private key.
fn public_of_private(private: &str) -> std::result::Result<(String, String), ()> {
    let key = russh::keys::PrivateKey::from_openssh(private).map_err(|_| ())?;
    let public = key.public_key().to_openssh().map_err(|_| ())?;
    let fp = ssh_fingerprint(&public).map_err(|_| ())?;
    Ok((public, fp))
}

fn fingerprint_of_private(private: &str) -> std::result::Result<String, ()> {
    public_of_private(private).map(|(_, fp)| fp)
}

